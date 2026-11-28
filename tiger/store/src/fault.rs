//! Fault injection under the store's file writes, for crash-safety fuzzing.
//!
//! Off unless the environment variable `TIGER_FAULT` is set when the first store opens; then it
//! costs one null-pointer check per write in libmdbx and one branch in tiger's own writers.
//! `TIGER_FAULT=seed=N,crash_after_writes=K,mode=torn|lose_unsynced`:
//!
//! - Every write to a store file counts: libmdbx's `pwrite`s to `mdbx.dat` (data pages and meta
//!   pages), and tiger's static-file writes. The `K`-th such write (1-based, counted from process
//!   start) crashes the process with `abort()`.
//! - `torn`: the `K`-th write first writes a random prefix of itself (from 0 bytes up to one
//!   byte short of the whole write), then the process aborts. Earlier writes stay as written.
//! - `lose_unsynced`: models power loss. Since the last fsync of each file, the old contents
//!   under every write are remembered. At the `K`-th write (which is not performed), the store
//!   undoes a random choice of those unsynced writes, either a random subset or every write
//!   after a random prefix, possibly tearing the boundary write, restoring the old bytes (and
//!   the old file length where nothing kept extends it), then aborts. Writes through a file
//!   opened with `O_DSYNC` (libmdbx's meta writes) count as synced when they return.
//!
//! - `enospc` (with `fail_writes=M`, default 1): writes `K` through `K+M-1` fail with `ENOSPC`
//!   instead of being performed (a libmdbx `pwrite`, a static-file write, a RocksDB batch), and
//!   the process keeps running; later writes succeed again, so the store's in-process heal and
//!   the node's retry can finish. The first failing write prints
//!   `tiger-fault: enospc at write K..K+M-1 phase="P" file=F`.
//! - `eio` (with `fail_writes=M`): as `enospc`, with `EIO` (a failing disk) instead; the line
//!   says `eio`.
//! - `fsync_eio` (with `fail_writes=M`): from write `K` on, the next `M` fsyncs of a store file
//!   fail with `EIO`, as Linux reports a failed writeback: the writes to that file since its
//!   last successful fsync are undone first (they never reached the disk, and the kernel drops
//!   the dirty pages), and a later fsync succeeds. The process keeps running. Each failing
//!   fsync prints `tiger-fault: fsync_eio at write N phase="P" file=F: lost L unsynced writes`.
//!
//! Before aborting it prints one line to stderr:
//! `tiger-fault: crash at write K mode=M phase="P" file=F offset=O len=L: D`, where `P` is the
//! phase the store set (such as `commit 17 mdbx-pages` or `commit 17 static-files`).

use std::{
    ffi::{c_int, c_void},
    fs::{self, File, OpenOptions},
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

/// What happens at the crash write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultMode {
    /// The crash write writes a random prefix of itself.
    Torn,
    /// Unsynced writes are partly lost.
    LoseUnsynced,
    /// Writes fail with `ENOSPC` and the process keeps running.
    Enospc,
    /// Writes fail with `EIO` and the process keeps running.
    Eio,
    /// Fsyncs fail with `EIO` after losing the file's unsynced writes; the process keeps running.
    FsyncEio,
}

/// A parsed `TIGER_FAULT` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultConfig {
    /// Seed of the random choices.
    pub seed: u64,
    /// The 1-based write that crashes.
    pub crash_after_writes: u64,
    /// The crash mode.
    pub mode: FaultMode,
    /// How many writes (`enospc`, `eio`) or fsyncs (`fsync_eio`) fail.
    pub fail_writes: u64,
}

impl FaultConfig {
    /// The environment variable that enables fault injection.
    pub const ENV: &'static str = "TIGER_FAULT";

    /// Parses `seed=N,crash_after_writes=K,mode=torn|lose_unsynced|enospc|eio|fsync_eio[,fail_writes=M]`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (mut seed, mut after, mut mode, mut fail_writes) = (0, None, None, 1);
        for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (key, value) = part.split_once('=').ok_or_else(|| format!("bad TIGER_FAULT part {part:?}"))?;
            match key {
                "seed" => seed = value.parse().map_err(|_| format!("bad seed {value:?}"))?,
                "crash_after_writes" => {
                    after = Some(value.parse().map_err(|_| format!("bad crash_after_writes {value:?}"))?)
                }
                "mode" => {
                    mode = Some(match value {
                        "torn" => FaultMode::Torn,
                        "lose_unsynced" => FaultMode::LoseUnsynced,
                        "enospc" => FaultMode::Enospc,
                        "eio" => FaultMode::Eio,
                        "fsync_eio" => FaultMode::FsyncEio,
                        _ => return Err(format!("bad mode {value:?}")),
                    })
                }
                "fail_writes" => fail_writes = value.parse().map_err(|_| format!("bad fail_writes {value:?}"))?,
                _ => return Err(format!("unknown TIGER_FAULT key {key:?}")),
            }
        }
        let crash_after_writes: u64 = after.ok_or("TIGER_FAULT needs crash_after_writes")?;
        if crash_after_writes == 0 {
            return Err("crash_after_writes must be at least 1".into());
        }
        if fail_writes == 0 {
            return Err("fail_writes must be at least 1".into());
        }
        Ok(Self { seed, crash_after_writes, mode: mode.ok_or("TIGER_FAULT needs mode")?, fail_writes })
    }
}

/// One unsynced write and the bytes it replaced.
#[derive(Debug)]
struct Undo {
    path: PathBuf,
    offset: u64,
    /// The bytes before the write, as far as the file extended.
    old: Vec<u8>,
    /// The write's length.
    len: u64,
}

/// The live fault state.
#[derive(Debug)]
struct State {
    config: FaultConfig,
    rng: u64,
    writes: u64,
    phase: String,
    /// Unsynced writes in order, oldest first.
    undo: Vec<Undo>,
    /// File length at its last sync (or when first written since), per file.
    synced_len: Vec<(PathBuf, u64)>,
    /// libmdbx file descriptors seen: (fd, tracked path or None, opened with O_DSYNC).
    fds: Vec<(c_int, Option<PathBuf>, bool)>,
    /// Fsyncs failed so far (`fsync_eio`).
    fsyncs_failed: u64,
}

static STATE: OnceLock<Option<Mutex<State>>> = OnceLock::new();

/// `ENOSPC` on Linux and macOS.
const ENOSPC: c_int = 28;
/// `EIO` on Linux and macOS.
const EIO: c_int = 5;

unsafe extern "C" {
    static mut tiger_hook_pwrite: Option<unsafe extern "C" fn(c_int, *const c_void, usize, u64) -> c_int>;
    static mut tiger_hook_fsync: Option<unsafe extern "C" fn(c_int) -> c_int>;
}

/// Fault injection entry points.
#[derive(Debug)]
pub struct Fault;

impl Fault {
    /// Reads `TIGER_FAULT` once and, when set, installs the libmdbx hooks. Panics on a malformed
    /// value: a fuzz run with a typo must not silently run without faults.
    pub fn init() {
        STATE.get_or_init(|| {
            let text = std::env::var(FaultConfig::ENV).ok()?;
            let config = FaultConfig::parse(&text).unwrap_or_else(|err| panic!("{err}"));
            // SAFETY: set once, before any store file is opened, from inside the OnceLock
            // initializer; libmdbx only reads them.
            unsafe {
                tiger_hook_pwrite = Some(mdbx_pwrite_hook);
                tiger_hook_fsync = Some(mdbx_fsync_hook);
            }
            let rng = config.seed ^ 0x9e37_79b9_7f4a_7c15;
            Some(Mutex::new(State {
                config,
                rng,
                writes: 0,
                phase: String::from("open"),
                undo: Vec::new(),
                synced_len: Vec::new(),
                fds: Vec::new(),
                fsyncs_failed: 0,
            }))
        });
    }

    fn state() -> Option<&'static Mutex<State>> {
        STATE.get().and_then(Option::as_ref)
    }

    /// Names what the store is doing, for the crash line.
    pub fn phase(commit: u64, step: &str) {
        if let Some(state) = Self::state() {
            state.lock().unwrap().phase = format!("commit {commit} {step}");
        }
    }

    /// Forgets libmdbx file descriptors (call after closing an environment; fds get reused).
    pub fn forget_fds() {
        if let Some(state) = Self::state() {
            state.lock().unwrap().fds.clear();
        }
    }

    /// Called by tiger's own writers before writing `buf` at `offset` of `path`; an error means
    /// the write must not be performed (`enospc`).
    pub fn before_write(path: &Path, offset: u64, buf: &[u8]) -> std::io::Result<()> {
        match Self::state() {
            Some(state) => state.lock().unwrap().write(path, offset, buf, false, "").map_err(std::io::Error::from_raw_os_error),
            None => Ok(()),
        }
    }

    /// Runs a durable write tiger cannot split into file writes (a synced RocksDB batch) as one
    /// counted write. At the crash write, `torn` and `lose_unsynced` both perform it or not at
    /// random (a torn WAL record is dropped by RocksDB's recovery; a synced one survives), then
    /// abort. Under `enospc`, a failing write is not performed and returns the error.
    pub fn opaque_write(name: &str, write: impl FnOnce()) -> std::io::Result<()> {
        let Some(state) = Self::state() else {
            write();
            return Ok(());
        };
        let mut state = state.lock().unwrap();
        state.writes += 1;
        if state.config.mode == FaultMode::FsyncEio {
            drop(state);
            write();
            return Ok(());
        }
        if matches!(state.config.mode, FaultMode::Enospc | FaultMode::Eio) {
            if let Err(errno) = state.enospc(Path::new(name)) {
                return Err(std::io::Error::from_raw_os_error(errno));
            }
            drop(state);
            write();
            return Ok(());
        }
        if state.writes != state.config.crash_after_writes {
            drop(state);
            write();
            return Ok(());
        }
        let performed = state.next() % 2 == 0;
        if performed {
            write();
        }
        eprintln!(
            "tiger-fault: crash at write {} mode={:?} phase=\"{}\" file={name} offset=0 len=0: batch {}",
            state.writes,
            state.config.mode,
            state.phase,
            if performed { "written" } else { "lost" },
        );
        std::process::abort();
    }

    /// Called by tiger's own writers after `path` was synced; an error means the sync failed
    /// (`fsync_eio`).
    pub fn after_sync(path: &Path) -> std::io::Result<()> {
        match Self::state() {
            Some(state) => state.lock().unwrap().synced(path).map_err(std::io::Error::from_raw_os_error),
            None => Ok(()),
        }
    }
}

impl State {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.rng = self.rng.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    fn file_len(path: &Path) -> u64 {
        fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }

    /// Counts a write; `Err(errno)` means it must fail instead of being performed.
    fn write(&mut self, path: &Path, offset: u64, buf: &[u8], durable: bool, detail: &str) -> Result<(), c_int> {
        self.writes += 1;
        if matches!(self.config.mode, FaultMode::Enospc | FaultMode::Eio) {
            return self.enospc(path);
        }
        if self.config.mode != FaultMode::FsyncEio && self.writes == self.config.crash_after_writes {
            self.crash(path, offset, buf, detail);
        }
        if !matches!(self.config.mode, FaultMode::LoseUnsynced | FaultMode::FsyncEio) || durable {
            return Ok(());
        }
        if !self.synced_len.iter().any(|(p, _)| p == path) {
            self.synced_len.push((path.to_path_buf(), Self::file_len(path)));
        }
        let mut old = vec![0; buf.len()];
        let read = File::open(path).and_then(|f| read_up_to(&f, &mut old, offset)).unwrap_or(0);
        old.truncate(read);
        self.undo.push(Undo { path: path.to_path_buf(), offset, old, len: buf.len() as u64 });
        Ok(())
    }

    /// `enospc` and `eio`: whether the write just counted falls in the failing range.
    fn enospc(&self, path: &Path) -> Result<(), c_int> {
        let first = self.config.crash_after_writes;
        let last = first + self.config.fail_writes - 1;
        if !(first..=last).contains(&self.writes) {
            return Ok(());
        }
        let (name, errno) = if self.config.mode == FaultMode::Eio { ("eio", EIO) } else { ("enospc", ENOSPC) };
        if self.writes == first {
            eprintln!("tiger-fault: {name} at write {first}..{last} phase=\"{}\" file={}", self.phase, path.display());
        }
        Err(errno)
    }

    /// A successful sync of `path`, or under `fsync_eio` a failed one (`Err(EIO)`) that lost the
    /// file's unsynced writes.
    fn synced(&mut self, path: &Path) -> Result<(), c_int> {
        let fail = self.config.mode == FaultMode::FsyncEio
            && self.writes >= self.config.crash_after_writes
            && self.fsyncs_failed < self.config.fail_writes
            && self.undo.iter().any(|u| u.path == path);
        if fail {
            self.fsyncs_failed += 1;
            let lost = self.undo.iter().filter(|u| u.path == path).count();
            for undo in self.undo.iter().rev().filter(|u| u.path == path) {
                let file = OpenOptions::new().write(true).open(&undo.path).expect("open undo file");
                let mut restore = undo.old.clone();
                restore.resize(undo.len as usize, 0);
                file.write_all_at(&restore, undo.offset).expect("undo write");
            }
            let kept_len = self.synced_len.iter().find(|(p, _)| p == path).map(|(_, len)| *len);
            if let (Some(len), Ok(file)) = (kept_len, OpenOptions::new().write(true).open(path)) {
                let _ = file.set_len(len);
            }
            eprintln!(
                "tiger-fault: fsync_eio at write {} phase=\"{}\" file={}: lost {lost} unsynced writes",
                self.writes,
                self.phase,
                path.display()
            );
        }
        self.undo.retain(|u| u.path != path);
        self.synced_len.retain(|(p, _)| p != path);
        if fail { Err(EIO) } else { Ok(()) }
    }

    fn crash(&mut self, path: &Path, offset: u64, buf: &[u8], detail: &str) -> ! {
        let what = match self.config.mode {
            FaultMode::Torn => {
                let prefix = self.below(buf.len() as u64) as usize;
                let file = OpenOptions::new().write(true).open(path).expect("open torn file");
                file.write_all_at(&buf[..prefix], offset).expect("torn write");
                format!("wrote {prefix} of {} bytes", buf.len())
            }
            FaultMode::LoseUnsynced => self.lose_unsynced(),
            FaultMode::Enospc | FaultMode::Eio | FaultMode::FsyncEio => unreachable!("{:?} never crashes", self.config.mode),
        };
        let phase = if detail.is_empty() { self.phase.clone() } else { format!("{} {detail}", self.phase) };
        eprintln!(
            "tiger-fault: crash at write {} mode={:?} phase=\"{phase}\" file={} offset={offset} len={}: {what}",
            self.writes,
            self.config.mode,
            path.display(),
            buf.len(),
        );
        std::process::abort();
    }

    /// Undoes a random choice of the unsynced writes and describes it.
    fn lose_unsynced(&mut self) -> String {
        let count = self.undo.len();
        let mut lost = vec![false; count];
        let mut torn = None;
        let how = if self.next() % 2 == 0 {
            for slot in &mut lost {
                *slot = self.next() % 2 == 0;
            }
            "subset"
        } else {
            let keep = self.below(count as u64 + 1) as usize;
            for slot in &mut lost[keep..] {
                *slot = true;
            }
            if keep < count && self.next() % 2 == 0 {
                torn = Some(keep);
            }
            "prefix"
        };
        // Undo newest first so overlapping writes restore the oldest bytes.
        for index in (0..count).rev() {
            if !lost[index] {
                continue;
            }
            let len = self.undo[index].len;
            let keep_bytes = if torn == Some(index) { self.below(len) as usize } else { 0 };
            let undo = &self.undo[index];
            let file = OpenOptions::new().write(true).open(&undo.path).expect("open undo file");
            let mut restore = undo.old.clone();
            restore.resize(undo.len as usize, 0);
            file.write_all_at(&restore[keep_bytes..], undo.offset + keep_bytes as u64).expect("undo write");
        }
        // Restore file lengths where no kept write extends past the synced length.
        for (path, synced) in &self.synced_len {
            let kept_end = self
                .undo
                .iter()
                .zip(&lost)
                .filter(|(u, lost)| &u.path == path && !**lost)
                .map(|(u, _)| u.offset + u.len)
                .max()
                .unwrap_or(0);
            let len = (*synced).max(kept_end);
            if let Ok(file) = OpenOptions::new().write(true).open(path) {
                let _ = file.set_len(len);
            }
        }
        let lost_count = lost.iter().filter(|l| **l).count();
        format!("lost {lost_count} of {count} unsynced writes ({how}{})", if torn.is_some() { ", boundary torn" } else { "" })
    }

    /// The tracked path of a libmdbx descriptor and whether writes through it are synchronous.
    fn fd_info(&mut self, fd: c_int) -> (Option<PathBuf>, bool) {
        if let Some((_, path, dsync)) = self.fds.iter().find(|(f, _, _)| *f == fd) {
            return (path.clone(), *dsync);
        }
        let path = fs::read_link(format!("/proc/self/fd/{fd}")).ok().filter(|p| p.ends_with("mdbx.dat"));
        // O_DSYNC is 0o10000 on Linux (x86-64 and aarch64); O_SYNC includes it.
        let dsync = fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))
            .ok()
            .and_then(|info| {
                let line = info.lines().find(|l| l.starts_with("flags:"))?;
                u64::from_str_radix(line.trim_start_matches("flags:").trim(), 8).ok()
            })
            .is_some_and(|flags| flags & 0o10000 != 0);
        self.fds.push((fd, path.clone(), dsync));
        (path, dsync)
    }
}

/// Reads as many bytes as the file holds at `offset`, up to `buf.len()`.
fn read_up_to(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        let n = file.read_at(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            break;
        }
        done += n;
    }
    Ok(done)
}

unsafe extern "C" fn mdbx_pwrite_hook(fd: c_int, buf: *const c_void, bytes: usize, offset: u64) -> c_int {
    let Some(state) = Fault::state() else { return 0 };
    let mut state = state.lock().unwrap();
    let (Some(path), dsync) = state.fd_info(fd) else { return 0 };
    // SAFETY: libmdbx passes a buffer of `bytes` bytes.
    let data = unsafe { std::slice::from_raw_parts(buf as *const u8, bytes) };
    // MDBX keeps its meta pages in the first three pages; tiger opens 4096-byte pages, and a
    // datadir with larger pages still has its metas below 3 * 65536.
    let detail = if offset < 3 * 4096 { "mdbx-meta" } else { "mdbx-pages" };
    match state.write(&path, offset, data, dsync, detail) {
        Ok(()) => 0,
        Err(errno) => errno,
    }
}

unsafe extern "C" fn mdbx_fsync_hook(fd: c_int) -> c_int {
    let Some(state) = Fault::state() else { return 0 };
    let mut state = state.lock().unwrap();
    match state.fd_info(fd) {
        (Some(path), _) => state.synced(&path).err().unwrap_or(0),
        (None, _) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_fuzzer_format() {
        let config = FaultConfig::parse("seed=7,crash_after_writes=12,mode=lose_unsynced").unwrap();
        assert_eq!(config, FaultConfig { seed: 7, crash_after_writes: 12, mode: FaultMode::LoseUnsynced, fail_writes: 1 });
        let config = FaultConfig::parse("seed=3,crash_after_writes=5,mode=enospc,fail_writes=4").unwrap();
        assert_eq!((config.mode, config.fail_writes), (FaultMode::Enospc, 4));
        assert!(FaultConfig::parse("crash_after_writes=5,mode=enospc,fail_writes=0").is_err());
        assert_eq!(FaultConfig::parse("crash_after_writes=5,mode=eio").unwrap().mode, FaultMode::Eio);
        assert_eq!(FaultConfig::parse("crash_after_writes=5,mode=fsync_eio,fail_writes=2").unwrap().fail_writes, 2);
        assert_eq!(FaultConfig::parse("crash_after_writes=1,mode=torn").unwrap().mode, FaultMode::Torn);
        assert!(FaultConfig::parse("seed=1,mode=torn").is_err());
        assert!(FaultConfig::parse("crash_after_writes=0,mode=torn").is_err());
        assert!(FaultConfig::parse("crash_after_writes=3,mode=sideways").is_err());
    }

    /// Child side of [`crash_keeps_every_acknowledged_commit`]: commits key `i` in transaction `i`
    /// and prints `committed i` after each commit returns, until the fault aborts it.
    #[test]
    fn crash_child() {
        let Ok(dir) = std::env::var("TIGER_FAULT_TEST_DIR") else { return };
        Fault::init();
        let env = crate::Env::open(Path::new(&dir), crate::EnvOptions::default()).unwrap();
        for i in 0u64..10_000 {
            Fault::phase(i, "mdbx");
            let mut txn = env.begin_rw().unwrap();
            let table = txn.create_table("T", false).unwrap();
            // A few hundred bytes per key over several pages so commits write data and meta pages.
            for j in 0..8u64 {
                txn.put(table, &[i.to_be_bytes(), j.to_be_bytes()].concat(), &[i as u8; 300]).unwrap();
            }
            txn.commit().unwrap();
            println!("committed {i}");
        }
    }

    #[test]
    fn crash_keeps_every_acknowledged_commit() {
        for (seed, after, mode) in [
            (1, 7, "torn"),
            (2, 40, "torn"),
            (3, 9, "lose_unsynced"),
            (4, 33, "lose_unsynced"),
            (5, 101, "lose_unsynced"),
            (6, 77, "torn"),
        ] {
            let dir = std::env::temp_dir().join(format!("tiger-fault-{seed}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "fault::tests::crash_child", "--nocapture", "--test-threads=1"])
                .env("TIGER_FAULT_TEST_DIR", &dir)
                .env(FaultConfig::ENV, format!("seed={seed},crash_after_writes={after},mode={mode}"))
                .output()
                .unwrap();
            assert!(!output.status.success(), "child must abort");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("tiger-fault: crash at write"), "{stderr}");
            let acknowledged = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|l| l.strip_prefix("committed ")?.parse::<u64>().ok())
                .max();
            let env = crate::Env::open(&dir, crate::EnvOptions::default()).unwrap();
            let txn = env.begin_ro().unwrap();
            let Some(table) = txn.open_table("T").unwrap() else {
                assert_eq!(acknowledged, None, "{stderr}");
                continue;
            };
            // Every acknowledged commit survives, whole, and commits are a prefix.
            let mut cursor = txn.cursor(table).unwrap();
            let mut count = 0u64;
            let mut entry = cursor.first().unwrap().map(|(k, v)| (k.to_vec(), v.to_vec()));
            while let Some((key, value)) = entry {
                let i = count / 8;
                assert_eq!(key, [i.to_be_bytes(), (count % 8).to_be_bytes()].concat());
                assert_eq!(value, vec![i as u8; 300]);
                count += 1;
                entry = cursor.next().unwrap().map(|(k, v)| (k.to_vec(), v.to_vec()));
            }
            assert_eq!(count % 8, 0, "a commit is all or nothing");
            let survived = (count / 8).checked_sub(1);
            assert!(survived >= acknowledged, "seed {seed}: acknowledged {acknowledged:?} survived {survived:?}: {stderr}");
            drop(cursor);
            drop(txn);
            drop(env);
            let _ = fs::remove_dir_all(&dir);
        }
    }
}
