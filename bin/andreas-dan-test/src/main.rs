//! One-shot MDBX debug tool.
//!
//! Probes a reth MDBX database directory to diagnose the intermittent
//! `-30784 / MDBX_INCOMPATIBLE` failure that affects ~1/3 of our hosts.
//!
//! The error means the flags (or page-size) recorded in the on-disk
//! environment header disagree with what we asked for at open time.
//! The most common root cause is a **page-size mismatch**: if one host
//! has a different OS page size (e.g. 4 KiB x86 vs 64 KiB ARM), the DB
//! gets created with that size and later opens on a different-page-size
//! host fail.  The second most common cause is the **WRITEMAP flag**: reth
//! enables `write_map()` in RW mode; if the env was somehow written without
//! it (or vice-versa) subsequent opens fail with the same error.
//!
//! Run with:
//!   andreas-dan-test --datadir /data/reth/db

use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use clap::Parser;
// reth_db re-exports the high-level helpers we care about at the crate root…
use reth_db::{
    ClientVersion, DatabaseEnv, DatabaseEnvKind,
    // …and everything from reth_libmdbx (Environment, Mode, PageSize, etc.)
    // is re-exported under reth_db::mdbx.
    mdbx::DatabaseArguments,
    open_db, open_db_read_only,
};
use reth_db_api::database::Database;
use tracing::{error, info, warn};

// ── CLI ───────────────────────────────────────────────────────────────────────

/// One-shot MDBX debug tool.
#[derive(Debug, Parser)]
#[command(name = "andreas-dan-test", about = "MDBX open debug tool")]
struct Args {
    /// Path to the reth MDBX database directory.
    ///
    /// This is the directory that contains the `mdbx.dat` / `mdbx.lck` files
    /// and the `database.version` file — i.e. the `db/` sub-directory of the
    /// reth data dir.
    #[arg(long)]
    datadir: PathBuf,
}

// ── entry point ───────────────────────────────────────────────────────────────

fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let path = &args.datadir;

    info!(path = %path.display(), "=== andreas-dan-test starting ===");

    // ── step 1: filesystem pre-flight ─────────────────────────────────────────
    // Before touching MDBX at all, check everything we can about the
    // directory: existence, ownership, permissions, which files are present,
    // and whether a stale lock file might block a RW open.
    check_filesystem(path);

    // ── step 2: OS page size ──────────────────────────────────────────────────
    // MDBX selects a page size at environment creation time.  reth's
    // `DatabaseArguments::new` passes `PageSize::Set(default_page_size())`
    // where `default_page_size()` returns `max(4096, os_page_size)`.
    // If the on-disk environment was created on a host with a *different* OS
    // page size, every subsequent open with a mismatching page size request
    // will return MDBX_INCOMPATIBLE (-30784).
    let os_page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let reth_default_page_size = std::cmp::max(4096, os_page_size as usize);
    info!(
        os_page_size       = os_page_size,
        reth_page_size_arg = reth_default_page_size,
        "OS page size (reth picks max(4096, this) as its MDBX page size argument)"
    );

    // ── step 3: reth database.version file ───────────────────────────────────
    // reth writes `database.version` (currently "2") the first time it calls
    // `create_db`.  A missing or mismatched version causes `init_db` to bail
    // *before* MDBX is touched.  `open_db` / `open_db_read_only` skip this
    // check, so it won't explain the -30784 error, but it's good to know.
    check_version_file(path);

    // ── step 4: try open_db_read_only ────────────────────────────────────────
    // Calls `DatabaseEnv::open(path, RO, args)`.
    // In RO mode reth does NOT call `inner_env.write_map()`, so the
    // MDBX_WRITEMAP flag is absent.  The geometry (page size, size range)
    // is still requested.  If this fails with -30784 the page-size mismatch
    // is almost certainly the cause.
    info!("--- Attempting open_db_read_only (RO, no WRITEMAP flag) ---");
    let ro_args = DatabaseArguments::new(ClientVersion::default());
    match open_db_read_only(path, ro_args) {
        Ok(db) => {
            info!("open_db_read_only: SUCCESS");
            // The stat() call returns the environment header info including
            // the page size that the DB was *actually* created with.  Compare
            // this to reth_page_size_arg above — any difference is the bug.
            probe_db(&db, DatabaseEnvKind::RO);
        }
        Err(e) => {
            error!(error = %format!("{e:#}"), "open_db_read_only: FAILED");
        }
    }

    // ── step 5: try open_db (RW + WRITEMAP) ──────────────────────────────────
    // This is what reth normally calls at startup.
    //
    // Internally `DatabaseEnv::open` does:
    //   1. `StorageLock::try_acquire(path)` — acquires `.lock` file
    //   2. `inner_env.write_map()`          — sets MDBX_WRITEMAP flag
    //   3. `inner_env.set_max_dbs(256)`
    //   4. `inner_env.set_geometry(args.geometry)` — incl. page size
    //   5. (optional) set log level, max readers, exclusive mode
    //   6. `inner_env.open(path)`           — the actual MDBX open
    //
    // MDBX_INCOMPATIBLE at step 6 means the on-disk env header has a flag
    // or page-size that contradicts what we set in steps 2-4.  The most
    // likely culprits are:
    //   a) WRITEMAP mismatch: env was created without WRITEMAP but we're
    //      opening with it, or vice-versa.
    //   b) Page-size mismatch: see step 2 above.
    //   c) max_dbs mismatch: env was created with a lower max_dbs value
    //      (unlikely but possible).
    info!("--- Attempting open_db (RW + WRITEMAP) ---");
    let rw_args = DatabaseArguments::new(ClientVersion::default());
    match open_db(path, rw_args) {
        Ok(db) => {
            info!("open_db (RW): SUCCESS");
            probe_db(&db, DatabaseEnvKind::RW);
        }
        Err(e) => {
            error!(error = %format!("{e:#}"), "open_db (RW): FAILED");
            // If RO succeeded but RW failed the most likely cause is the
            // WRITEMAP flag mismatch.  If both failed, it's more likely a
            // page-size mismatch or corrupted env header.
        }
    }

    info!("=== andreas-dan-test done ===");
    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Logs every piece of filesystem metadata we can gather about `path` and its
/// immediate children.
fn check_filesystem(path: &Path) {
    info!(path = %path.display(), "--- Filesystem check ---");

    match path.try_exists() {
        Ok(true) => info!(path = %path.display(), "path exists"),
        Ok(false) => {
            warn!(path = %path.display(), "path does NOT exist — nothing to open");
            return;
        }
        Err(e) => {
            error!(error = %e, path = %path.display(), "could not stat path");
            return;
        }
    }

    log_path_meta(path);

    // Walk directory one level deep to see exactly which MDBX files are
    // present.  We're looking for:
    //   mdbx.dat  — the main data file (should always exist)
    //   mdbx.lck  — the lock file (exists while a writer is open)
    //   database.version — reth's own version sentinel
    match fs::read_dir(path) {
        Err(e) => error!(error = %e, path = %path.display(), "could not read directory"),
        Ok(entries) => {
            for entry in entries.flatten() {
                log_path_meta(&entry.path());
            }
        }
    }

    // A stale `mdbx.lck` can prevent a new writer from acquiring the env.
    // reth acquires its own `<datadir>/lock` file *before* calling MDBX,
    // so a crash can leave both files behind.
    let mdbx_lck = path.join("mdbx.lck");
    if mdbx_lck.exists() {
        // Size 0 means the env was cleanly closed.  Non-zero means a writer
        // was (or still is) active.
        let size = fs::metadata(&mdbx_lck).map(|m| m.len()).unwrap_or(0);
        if size > 0 {
            warn!(
                lck  = %mdbx_lck.display(),
                size = size,
                "mdbx.lck is NON-EMPTY — a previous writer may not have exited cleanly. \
                 This can cause MDB_BUSY or MDBX_INCOMPATIBLE on some MDBX builds."
            );
        } else {
            info!(lck = %mdbx_lck.display(), "mdbx.lck present and empty (env was cleanly closed)");
        }
    } else {
        info!("mdbx.lck absent — no writer has opened this env yet, or it was cleaned up");
    }

    // reth also writes its own advisory lock file (not MDBX's).
    // The file is named "lock" (no dot prefix), stored in the db directory.
    // It contains the PID and process start time of the owning reth process.
    let reth_lock = path.join("lock");
    if reth_lock.exists() {
        let contents = fs::read_to_string(&reth_lock).unwrap_or_default();
        warn!(
            lock     = %reth_lock.display(),
            contents = %contents.trim(),
            "reth lock file is present — another reth process may own this database (RW opens will fail)"
        );
    }
}

/// Logs metadata (size, mode, uid, gid, inode) for a single path.
fn log_path_meta(p: &Path) {
    match fs::metadata(p) {
        Err(e) => error!(error = %e, path = %p.display(), "could not stat"),
        Ok(m) => {
            info!(
                path    = %p.display(),
                is_dir  = m.is_dir(),
                size    = m.len(),
                mode    = format!("{:o}", m.permissions().mode()),
                uid     = m.uid(),
                gid     = m.gid(),
                inode   = m.ino(),
                "path metadata"
            );
        }
    }
}

/// Reads and logs the reth `database.version` file.
///
/// reth's `create_db` path (called by `init_db`) writes this file.
/// `open_db` and `open_db_read_only` do NOT check it — they go straight
/// to MDBX — so a wrong version here is not the direct cause of -30784.
fn check_version_file(path: &Path) {
    info!("--- database.version check ---");
    let vfile = path.join("database.version");
    match fs::read_to_string(&vfile) {
        Ok(contents) => {
            let trimmed = contents.trim();
            // DB_VERSION is currently 2 in reth v1.11.1.
            let matches = trimmed == "2";
            if matches {
                info!(version = %trimmed, "database.version OK (matches reth v1.11.1 expected value of 2)");
            } else {
                warn!(
                    found    = %trimmed,
                    expected = 2,
                    "database.version MISMATCH — init_db will refuse to open but \
                     open_db / open_db_read_only will still proceed to MDBX"
                );
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Absence is fine if mdbx.dat also doesn't exist (first run).
            // If mdbx.dat *does* exist but the version file is missing, reth's
            // create_db will create it (MissingFile → create_db_version_file).
            warn!("database.version MISSING — will be created on first init_db call");
        }
        Err(e) => {
            error!(error = %e, path = %vfile.display(), "could not read database.version");
        }
    }
}

/// After a successful open, probe the environment and log key statistics.
///
/// The critical field is `stat().page_size()`: this is the page size that
/// the environment was **actually created with**, stored in the env header.
/// Compare it to the `reth_page_size_arg` logged in main — any difference
/// means the next open call (with the mismatching arg) will return -30784.
fn probe_db(db: &DatabaseEnv, kind: DatabaseEnvKind) {
    let mode = match kind {
        DatabaseEnvKind::RO => "RO",
        DatabaseEnvKind::RW => "RW",
    };

    // stat() on the MDBX environment returns the env-header metadata.
    // page_size here is the actual on-disk page size, not what we requested.
    match db.stat() {
        Ok(s) => {
            info!(
                mode      = mode,
                page_size = s.page_size(),
                "db stat — page_size is what the env was CREATED with (compare to reth_page_size_arg above)"
            );
        }
        Err(e) => warn!(mode = mode, error = %e, "could not read db stat"),
    }

    // freelist: number of freed pages available for reuse.  A very large
    // freelist can cause slow opens; not directly related to -30784.
    match db.freelist() {
        Ok(n) => info!(mode = mode, freelist_pages = n, "db freelist"),
        Err(e) => warn!(mode = mode, error = %e, "could not read freelist"),
    }

    // Try starting a read transaction.  If this fails the env opened but is
    // internally corrupt in some way.
    match db.tx() {
        Ok(_tx) => info!(mode = mode, "read transaction started successfully — env is readable"),
        Err(e) => error!(mode = mode, error = %e, "could not begin read transaction"),
    }
}
