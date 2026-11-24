//! base-reth's MDBX tables and the datadir layout around them.
//!
//! Table names and `DUPSORT` flags are reth's `tables!` list (crates/storage/db-api/src/tables at
//! base/reth 2b20e42), in the same order. Tiger creates every one of them, as reth does, so either
//! client opens a datadir the other made. The `Tiger*` tables are tiger's own (reth opens tables
//! by name and ignores others): `TigerNodes` holds a partial state's trie nodes as RLP by
//! path (`0 ++ packed path` for the account trie, `1 ++ hashed address ++ packed path` for storage
//! tries), `TigerWipes` (`hashed address ++ block`) and `TigerWipedStorage`
//! (`block ++ hashed address` -> `hashed slot ++ value` duplicates) keep the storage a wipe
//! removed, which reth's v2 changesets do not, so unwinds and historical roots restore it.
//! `TigerForkchoice` holds the node's forkchoice heads (reth keeps only the safe and finalized
//! numbers, in `ChainState`).

use std::{fs, io, path::Path};

use crate::{Dbi, Env, EnvOptions, MdbxError, Ro, Rw, Txn};

macro_rules! tables {
    ($($name:ident: $dupsort:expr),* $(,)?) => {
        /// A table of the datadir's MDBX file.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Table {
            $(
                #[doc = concat!("reth's `", stringify!($name), "` table.")]
                $name,
            )*
        }

        impl Table {
            /// Every table, in reth's order (tiger's last).
            pub const ALL: &'static [Self] = &[$(Self::$name),*];

            /// The table name in the file.
            pub const fn name(self) -> &'static str {
                match self { $(Self::$name => stringify!($name)),* }
            }

            /// Whether the table is `DUPSORT`.
            pub const fn dupsort(self) -> bool {
                match self { $(Self::$name => $dupsort),* }
            }
        }
    };
}

tables! {
    CanonicalHeaders: false,
    HeaderTerminalDifficulties: false,
    HeaderNumbers: false,
    Headers: false,
    BlockBodyIndices: false,
    BlockOmmers: false,
    BlockWithdrawals: false,
    BlockAccessLists: false,
    BlockAccessListBlockNumbers: false,
    Transactions: false,
    TransactionHashNumbers: false,
    TransactionBlocks: false,
    Receipts: false,
    Bytecodes: false,
    PlainAccountState: false,
    PlainStorageState: true,
    AccountsHistory: false,
    StoragesHistory: false,
    AccountChangeSets: true,
    StorageChangeSets: true,
    HashedAccounts: false,
    HashedStorages: true,
    AccountsTrie: false,
    StoragesTrie: true,
    TransactionSenders: false,
    StageCheckpoints: false,
    StageCheckpointProgresses: false,
    PruneCheckpoints: false,
    VersionHistory: false,
    ChainState: false,
    Metadata: false,
    TigerNodes: false,
    TigerWipes: false,
    TigerWipedStorage: true,
    TigerForkchoice: false,
}

/// The `database.version` file reth checks next to `mdbx.dat`.
pub const DB_VERSION_FILE: &str = "database.version";
/// The database version reth 2b20e42 writes and accepts.
pub const DB_VERSION: u64 = 2;

/// An open datadir MDBX file with every table's handle.
#[derive(Debug, Clone)]
pub struct Db {
    /// The environment.
    pub env: Env,
    /// Table handles by `Table as usize`.
    pub dbis: Vec<Dbi>,
}

/// Failure to open a datadir database.
#[derive(Debug)]
pub enum OpenError {
    /// MDBX refused.
    Mdbx(MdbxError),
    /// File system error.
    Io(io::Error),
    /// The `database.version` file holds another version.
    Version(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mdbx(err) => write!(f, "{err}"),
            Self::Io(err) => write!(f, "{err}"),
            Self::Version(text) => write!(f, "unsupported database version: {text}"),
        }
    }
}

impl std::error::Error for OpenError {}

impl From<MdbxError> for OpenError {
    fn from(err: MdbxError) -> Self {
        Self::Mdbx(err)
    }
}

impl From<io::Error> for OpenError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl Db {
    /// Opens (creating if needed) the MDBX database in `dir` (the datadir's `db/`), creating
    /// missing tables and the version file as reth does.
    pub fn open(dir: &Path) -> Result<Self, OpenError> {
        crate::Fault::init();
        fs::create_dir_all(dir)?;
        let version_file = dir.join(DB_VERSION_FILE);
        match fs::read_to_string(&version_file) {
            Ok(text) if text.trim().parse::<u64>().ok() == Some(DB_VERSION) => {}
            Ok(text) => return Err(OpenError::Version(text)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                fs::write(&version_file, DB_VERSION.to_string())?;
            }
            Err(err) => return Err(err.into()),
        }
        crate::Fault::forget_fds();
        let env = Env::open(dir, EnvOptions::default())?;
        let txn = env.begin_rw()?;
        let dbis = Table::ALL.iter().map(|t| txn.create_table(t.name(), t.dupsort())).collect::<Result<Vec<_>, _>>()?;
        txn.commit()?;
        Ok(Self { env, dbis })
    }

    /// The handle of `table`.
    pub fn dbi(&self, table: Table) -> Dbi {
        self.dbis[table as usize]
    }

    /// Begins a read transaction.
    pub fn read(&self) -> Result<Txn<Ro>, MdbxError> {
        self.env.begin_ro()
    }

    /// Begins the write transaction.
    pub fn write(&self) -> Result<Txn<Rw>, MdbxError> {
        self.env.begin_rw()
    }
}
