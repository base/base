//! Static files: base-reth's append-only nippy-jar segments for headers, transactions, receipts,
//! senders and the account and storage changesets.
//!
//! Each segment is split into files of `blocks_per_file` blocks (500 000 by default), named
//! `static_file_<segment>_<start>_<end>`. One jar is four files:
//!
//! - the data file: column values back to back (LZ4 block-compressed per value for headers);
//! - `.off`: one byte `8`, then little-endian `u64` offsets: the start of every value and, last,
//!   the data length;
//! - `.conf`: the bincode-encoded `NippyJar<SegmentHeader>` (rows, ranges, compressor), replaced
//!   atomically (write `.tmp`, fsync, rename, fsync the directory);
//! - `.csoff` (changesets only): one `(first row, row count)` pair of little-endian `u64`s per
//!   block.
//!
//! A commit appends and fsyncs data, then offsets, then `.csoff`, then replaces `.conf`, so the
//! configuration never counts rows that are not durable. Opening heals a jar exactly as reth's
//! `NippyJarChecker` does: offsets beyond the configured rows and data beyond the last offset are
//! cut, and a configuration that counts more rows than the offsets hold is lowered. Reads use
//! only committed rows. This is a hand-written copy of the behavior of reth's `nippy-jar` and
//! `static-file` crates at base/reth 2b20e42 (see the README).

use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::RwLock,
};

use crate::Fault;

/// Blocks per static file in a new datadir (reth's `DEFAULT_BLOCKS_PER_STATIC_FILE`).
pub const BLOCKS_PER_FILE: u64 = 500_000;

/// The nippy-jar format version.
const JAR_VERSION: u64 = 1;
/// Bytes per offset, the first byte of every `.off` file.
const OFFSET_SIZE: u64 = 8;
/// Bytes per `.csoff` record.
const CSOFF_SIZE: u64 = 16;

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// A static-file segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Segment {
    /// Headers: header, total difficulty and hash columns; one row per block.
    Headers,
    /// Transactions, one row per transaction.
    Transactions,
    /// Receipts, one row per transaction.
    Receipts,
    /// Transaction senders, one row per transaction.
    TransactionSenders,
    /// Account changesets, rows grouped per block by `.csoff`.
    AccountChangeSets,
    /// Storage changesets, rows grouped per block by `.csoff`.
    StorageChangeSets,
}

impl Segment {
    /// Every segment, in reth's order.
    pub const ALL: [Self; 6] = [
        Self::Headers,
        Self::Transactions,
        Self::Receipts,
        Self::TransactionSenders,
        Self::AccountChangeSets,
        Self::StorageChangeSets,
    ];

    /// The name in file names.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Headers => "headers",
            Self::Transactions => "transactions",
            Self::Receipts => "receipts",
            Self::TransactionSenders => "transaction-senders",
            Self::AccountChangeSets => "account-change-sets",
            Self::StorageChangeSets => "storage-change-sets",
        }
    }

    /// Columns per row.
    pub const fn columns(self) -> u64 {
        match self {
            Self::Headers => 3,
            _ => 1,
        }
    }

    /// Whether rows are transactions.
    pub const fn is_tx_based(self) -> bool {
        matches!(self, Self::Transactions | Self::Receipts | Self::TransactionSenders)
    }

    /// Whether rows are grouped per block by `.csoff`.
    pub const fn is_change_based(self) -> bool {
        matches!(self, Self::AccountChangeSets | Self::StorageChangeSets)
    }

    const fn index(self) -> u32 {
        self as u32
    }

    fn from_index(index: u32) -> io::Result<Self> {
        Self::ALL.get(index as usize).copied().ok_or_else(|| invalid(format!("unknown segment {index}")))
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The file name of the jar covering `range`.
    pub fn file_name(self, range: Range) -> String {
        format!("static_file_{}_{}_{}", self.name(), range.start, range.end)
    }
}

/// An inclusive range of block or transaction numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    /// First number.
    pub start: u64,
    /// Last number.
    pub end: u64,
}

impl Range {
    /// `start..=end`.
    pub const fn new(start: u64, end: u64) -> Self {
        Self { start, end }
    }

    /// The fixed file range holding `block`.
    pub const fn fixed(block: u64, blocks_per_file: u64) -> Self {
        let start = block / blocks_per_file * blocks_per_file;
        Self { start, end: start + blocks_per_file - 1 }
    }

    /// Numbers in the range.
    pub const fn len(self) -> u64 {
        self.end - self.start + 1
    }

    /// Whether `n` is in the range.
    pub const fn contains(self, n: u64) -> bool {
        self.start <= n && n <= self.end
    }
}

/// reth's `SegmentHeader`: what a jar covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentHeader {
    /// The fixed block range of the file.
    pub expected: Range,
    /// Blocks with data, `None` when empty.
    pub blocks: Option<Range>,
    /// Transactions with data (transaction segments), `None` when empty.
    pub txs: Option<Range>,
    /// The segment.
    pub segment: Segment,
    /// `.csoff` records (changeset segments).
    pub changeset_offsets_len: u64,
}

/// The `.conf` file: reth's bincode-encoded `NippyJar<SegmentHeader>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JarConfig {
    /// The segment header.
    pub header: SegmentHeader,
    /// Committed rows.
    pub rows: u64,
    /// Whether values are LZ4-compressed (headers).
    pub lz4: bool,
    /// Largest uncompressed row, in bytes.
    pub max_row_size: u64,
}

impl JarConfig {
    /// An empty jar for `segment` covering the file range `expected`.
    pub const fn new(segment: Segment, expected: Range) -> Self {
        Self {
            header: SegmentHeader { expected, blocks: None, txs: None, segment, changeset_offsets_len: 0 },
            rows: 0,
            lz4: matches!(segment, Segment::Headers),
            max_row_size: 0,
        }
    }

    /// bincode 1 (fixed-width little-endian integers, `u8` option tags, `u32` enum tags).
    pub fn encode(&self) -> Vec<u8> {
        fn range(out: &mut Vec<u8>, r: Option<Range>) {
            match r {
                None => out.push(0),
                Some(r) => {
                    out.push(1);
                    out.extend_from_slice(&r.start.to_le_bytes());
                    out.extend_from_slice(&r.end.to_le_bytes());
                }
            }
        }
        let h = &self.header;
        let mut out = Vec::with_capacity(96);
        out.extend_from_slice(&JAR_VERSION.to_le_bytes());
        out.extend_from_slice(&h.expected.start.to_le_bytes());
        out.extend_from_slice(&h.expected.end.to_le_bytes());
        range(&mut out, h.blocks);
        range(&mut out, h.txs);
        out.extend_from_slice(&h.segment.index().to_le_bytes());
        if h.segment.is_change_based() {
            out.extend_from_slice(&h.changeset_offsets_len.to_le_bytes());
        }
        out.extend_from_slice(&h.segment.columns().to_le_bytes());
        out.extend_from_slice(&self.rows.to_le_bytes());
        if self.lz4 {
            out.push(1);
            out.extend_from_slice(&1u32.to_le_bytes());
        } else {
            out.push(0);
        }
        out.extend_from_slice(&self.max_row_size.to_le_bytes());
        out
    }

    /// Decodes a `.conf` file.
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        struct R<'a>(&'a [u8]);
        impl R<'_> {
            fn take<const N: usize>(&mut self) -> io::Result<[u8; N]> {
                if self.0.len() < N {
                    return Err(invalid("short static file config".into()));
                }
                let (head, rest) = self.0.split_at(N);
                self.0 = rest;
                Ok(head.try_into().expect("N bytes"))
            }
            fn u64(&mut self) -> io::Result<u64> {
                self.take::<8>().map(u64::from_le_bytes)
            }
            fn tag(&mut self) -> io::Result<bool> {
                match self.take::<1>()?[0] {
                    0 => Ok(false),
                    1 => Ok(true),
                    t => Err(invalid(format!("bad option tag {t}"))),
                }
            }
            fn range(&mut self) -> io::Result<Option<Range>> {
                Ok(if self.tag()? { Some(Range::new(self.u64()?, self.u64()?)) } else { None })
            }
        }
        let mut r = R(bytes);
        let version = r.u64()?;
        if version != JAR_VERSION {
            return Err(invalid(format!("unsupported nippy-jar version {version}")));
        }
        let expected = Range::new(r.u64()?, r.u64()?);
        let blocks = r.range()?;
        let txs = r.range()?;
        let segment = Segment::from_index(u32::from_le_bytes(r.take::<4>()?))?;
        let changeset_offsets_len = if segment.is_change_based() { r.u64()? } else { 0 };
        let columns = r.u64()?;
        if columns != segment.columns() {
            return Err(invalid(format!("{} jar with {columns} columns", segment.name())));
        }
        let rows = r.u64()?;
        let lz4 = if r.tag()? {
            match u32::from_le_bytes(r.take::<4>()?) {
                1 => true,
                v => return Err(invalid(format!("unsupported compressor variant {v}"))),
            }
        } else {
            false
        };
        let max_row_size = r.u64()?;
        if !r.0.is_empty() {
            return Err(invalid("trailing bytes in static file config".into()));
        }
        Ok(Self { header: SegmentHeader { expected, blocks, txs, segment, changeset_offsets_len }, rows, lz4, max_row_size })
    }
}

/// LZ4 block format, as `lz4_flex` writes and reads it (no size prefix).
#[derive(Debug)]
pub struct Lz4;

impl Lz4 {
    /// Encodes `src` as one literal-only sequence: valid LZ4 that any decoder (including reth's)
    /// reads, at the cost of no compression.
    pub fn compress(src: &[u8], out: &mut Vec<u8>) {
        let len = src.len();
        if len < 15 {
            out.push((len as u8) << 4);
        } else {
            out.push(0xf0);
            let mut rest = len - 15;
            while rest >= 255 {
                out.push(255);
                rest -= 255;
            }
            out.push(rest as u8);
        }
        out.extend_from_slice(src);
    }

    /// Decodes an LZ4 block, refusing output beyond `limit` bytes.
    pub fn decompress(src: &[u8], limit: usize, out: &mut Vec<u8>) -> io::Result<()> {
        let bad = || invalid("corrupt lz4 block".into());
        let start = out.len();
        let mut i = 0;
        let read_len = |i: &mut usize, mut len: usize| -> io::Result<usize> {
            if len == 15 {
                loop {
                    let b = *src.get(*i).ok_or_else(bad)?;
                    *i += 1;
                    len += b as usize;
                    if b != 255 {
                        break;
                    }
                }
            }
            Ok(len)
        };
        while i < src.len() {
            let token = src[i];
            i += 1;
            let literals = read_len(&mut i, (token >> 4) as usize)?;
            let end = i.checked_add(literals).filter(|e| *e <= src.len()).ok_or_else(bad)?;
            out.extend_from_slice(&src[i..end]);
            i = end;
            if i == src.len() {
                break;
            }
            if i + 2 > src.len() {
                return Err(bad());
            }
            let offset = u16::from_le_bytes([src[i], src[i + 1]]) as usize;
            i += 2;
            let length = read_len(&mut i, (token & 0x0f) as usize)? + 4;
            let produced = out.len() - start;
            if offset == 0 || offset > produced || produced + length > limit {
                return Err(bad());
            }
            let from = out.len() - offset;
            for k in 0..length {
                let byte = out[from + k];
                out.push(byte);
            }
            if out.len() - start > limit {
                return Err(bad());
            }
        }
        if out.len() - start > limit {
            return Err(bad());
        }
        Ok(())
    }
}

fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    file.read_exact_at(buf, offset)
}

fn read_u64_at(file: &File, offset: u64) -> io::Result<u64> {
    let mut buf = [0; 8];
    read_exact_at(file, &mut buf, offset)?;
    Ok(u64::from_le_bytes(buf))
}

fn write_at(path: &Path, file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    Fault::before_write(path, offset, buf)?;
    file.write_all_at(buf, offset)
}

fn sync(path: &Path, file: &File) -> io::Result<()> {
    file.sync_all()?;
    Fault::after_sync(path)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// One committed jar, open for reading and appending.
#[derive(Debug)]
pub struct Jar {
    /// The data file's path; the others add `.off`, `.conf`, `.csoff`.
    pub path: PathBuf,
    /// The committed configuration.
    pub config: JarConfig,
    data: File,
    offsets: File,
    csoff: Option<File>,
}

impl Jar {
    fn open_files(path: &Path, change_based: bool) -> io::Result<(File, File, Option<File>)> {
        let open = |p: &Path| OpenOptions::new().read(true).write(true).create(true).truncate(false).open(p);
        let data = open(path)?;
        let offsets = open(&with_suffix(path, ".off"))?;
        let csoff = if change_based { Some(open(&with_suffix(path, ".csoff"))?) } else { None };
        Ok((data, offsets, csoff))
    }

    /// Creates an empty jar: offsets `[8, 0]` and the configuration, all synced.
    fn create(path: PathBuf, config: JarConfig) -> io::Result<Self> {
        let (data, offsets, csoff) = Self::open_files(&path, config.header.segment.is_change_based())?;
        data.set_len(0)?;
        offsets.set_len(0)?;
        let mut init = vec![OFFSET_SIZE as u8];
        init.extend_from_slice(&0u64.to_le_bytes());
        let off_path = with_suffix(&path, ".off");
        write_at(&off_path, &offsets, &init, 0)?;
        sync(&off_path, &offsets)?;
        if let Some(csoff) = &csoff {
            csoff.set_len(0)?;
        }
        let jar = Self { path, config, data, offsets, csoff };
        jar.write_config()?;
        Ok(jar)
    }

    /// Opens a jar and heals it as reth's checker does.
    fn open(path: PathBuf) -> io::Result<Self> {
        let conf_path = with_suffix(&path, ".conf");
        let mut config = JarConfig::decode(&fs::read(&conf_path)?)?;
        let change_based = config.header.segment.is_change_based();
        let (data, offsets, csoff) = Self::open_files(&path, change_based)?;
        let mut jar = Self { path, config: config.clone(), data, offsets, csoff };
        let columns = config.header.segment.columns();
        let mut healed = false;
        // Offsets file: 1 + 8 * (rows * columns + 1) bytes.
        let expected = 1 + OFFSET_SIZE * (config.rows * columns + 1);
        let actual = jar.offsets.metadata()?.len();
        if actual < 1 + OFFSET_SIZE {
            return Err(invalid(format!("{}: offsets file too short", jar.path.display())));
        }
        if actual > expected {
            jar.offsets.set_len(expected)?;
            jar.offsets.sync_all()?;
        } else if actual < expected {
            config.rows = (actual - 1 - OFFSET_SIZE) / OFFSET_SIZE / columns;
            jar.offsets.set_len(1 + OFFSET_SIZE * (config.rows * columns + 1))?;
            jar.offsets.sync_all()?;
            healed = true;
        }
        // Data file: cut beyond the last offset; if it is shorter, drop rows until it fits.
        loop {
            let values = config.rows * columns;
            let last = read_u64_at(&jar.offsets, 1 + OFFSET_SIZE * values)?;
            let data_len = jar.data.metadata()?.len();
            if last < data_len {
                jar.data.set_len(last)?;
                jar.data.sync_all()?;
            }
            if last <= data_len {
                break;
            }
            assert!(config.rows > 0, "the first offset is 0");
            config.rows -= 1;
            jar.offsets.set_len(1 + OFFSET_SIZE * ((config.rows) * columns + 1))?;
            jar.offsets.sync_all()?;
            healed = true;
        }
        if let Some(csoff) = &jar.csoff {
            // Cut uncommitted records, then drop records whose rows were lost.
            let records = csoff.metadata()?.len() / CSOFF_SIZE;
            let mut len = config.header.changeset_offsets_len.min(records);
            while len > 0 {
                let first = read_u64_at(csoff, (len - 1) * CSOFF_SIZE)?;
                let count = read_u64_at(csoff, (len - 1) * CSOFF_SIZE + 8)?;
                if first + count <= config.rows {
                    break;
                }
                len -= 1;
            }
            if csoff.metadata()?.len() != len * CSOFF_SIZE {
                csoff.set_len(len * CSOFF_SIZE)?;
                csoff.sync_all()?;
            }
            if len != config.header.changeset_offsets_len {
                config.header.changeset_offsets_len = len;
                config.header.blocks = config.header.blocks.and_then(|b| {
                    (len > 0).then(|| Range::new(b.start, b.start + len - 1))
                });
                healed = true;
            }
        }
        if healed {
            // Keep the ranges within the surviving rows where the rows determine them; a
            // transaction segment's block range is fixed by the datadir heal, which knows the
            // block bodies.
            let rows = config.rows;
            let h = &mut config.header;
            match h.segment {
                Segment::Headers => h.blocks = h.blocks.and_then(|b| (rows > 0).then(|| Range::new(b.start, b.start + rows - 1))),
                s if s.is_tx_based() => h.txs = h.txs.and_then(|t| (rows > 0).then(|| Range::new(t.start, t.start + rows - 1))),
                _ => {}
            }
        }
        jar.config = config;
        if healed {
            jar.write_config()?;
        }
        Ok(jar)
    }

    /// Replaces `.conf` atomically.
    fn write_config(&self) -> io::Result<()> {
        let conf = with_suffix(&self.path, ".conf");
        let tmp = conf.with_extension("tmp");
        let bytes = self.config.encode();
        let file = File::create(&tmp)?;
        write_at(&tmp, &file, &bytes, 0)?;
        sync(&tmp, &file)?;
        fs::rename(&tmp, &conf)?;
        let dir = conf.parent().expect("static file in a directory");
        File::open(dir)?.sync_all()?;
        Ok(())
    }

    /// The byte range of value `index` (row * columns + column).
    fn value_range(&self, index: u64) -> io::Result<(u64, u64)> {
        let mut buf = [0; 16];
        read_exact_at(&self.offsets, &mut buf, 1 + OFFSET_SIZE * index)?;
        let start = u64::from_le_bytes(buf[..8].try_into().expect("8"));
        let end = u64::from_le_bytes(buf[8..].try_into().expect("8"));
        if start > end {
            return Err(invalid(format!("{}: offsets out of order at {index}", self.path.display())));
        }
        Ok((start, end))
    }

    /// Appends column `column` of committed row `row` (0-based within this jar) to `out`.
    pub fn value(&self, row: u64, column: u64, out: &mut Vec<u8>) -> io::Result<()> {
        let columns = self.config.header.segment.columns();
        assert!(column < columns);
        if row >= self.config.rows {
            return Err(invalid(format!("{}: row {row} beyond {}", self.path.display(), self.config.rows)));
        }
        let (start, end) = self.value_range(row * columns + column)?;
        let mut raw = vec![0; (end - start) as usize];
        read_exact_at(&self.data, &mut raw, start)?;
        if self.config.lz4 {
            Lz4::decompress(&raw, self.config.max_row_size as usize, out)
        } else {
            out.extend_from_slice(&raw);
            Ok(())
        }
    }

    /// The `(first row, row count)` of `block`'s changes, for changeset jars.
    pub fn changeset(&self, block: u64) -> io::Result<Option<(u64, u64)>> {
        let csoff = self.csoff.as_ref().expect("changeset jar");
        let Some(blocks) = self.config.header.blocks else { return Ok(None) };
        if !blocks.contains(block) || block - blocks.start >= self.config.header.changeset_offsets_len {
            return Ok(None);
        }
        let at = (block - blocks.start) * CSOFF_SIZE;
        Ok(Some((read_u64_at(csoff, at)?, read_u64_at(csoff, at + 8)?)))
    }

    /// The data length the committed offsets end at.
    fn data_end(&self) -> io::Result<u64> {
        read_u64_at(&self.offsets, 1 + OFFSET_SIZE * self.config.rows * self.config.header.segment.columns())
    }

    /// Appends pending rows and commits: data, offsets, `.csoff`, then `.conf`, each synced.
    fn append(&mut self, pending: &Pending) -> io::Result<()> {
        let columns = self.config.header.segment.columns();
        assert_eq!(pending.ends.len() as u64, pending.rows * columns);
        let data_end = self.data_end()?;
        if !pending.data.is_empty() {
            write_at(&self.path, &self.data, &pending.data, data_end)?;
            sync(&self.path, &self.data)?;
        }
        if !pending.ends.is_empty() {
            let mut bytes = Vec::with_capacity(pending.ends.len() * 8);
            for end in &pending.ends {
                bytes.extend_from_slice(&(data_end + end).to_le_bytes());
            }
            let off_path = with_suffix(&self.path, ".off");
            let at = 1 + OFFSET_SIZE * (self.config.rows * columns + 1);
            write_at(&off_path, &self.offsets, &bytes, at)?;
            sync(&off_path, &self.offsets)?;
        }
        if !pending.csoff.is_empty() {
            let csoff = self.csoff.as_ref().expect("changeset jar");
            let mut bytes = Vec::with_capacity(pending.csoff.len() * 16);
            for (first, count) in &pending.csoff {
                bytes.extend_from_slice(&(self.config.rows + first).to_le_bytes());
                bytes.extend_from_slice(&count.to_le_bytes());
            }
            let path = with_suffix(&self.path, ".csoff");
            write_at(&path, csoff, &bytes, self.config.header.changeset_offsets_len * CSOFF_SIZE)?;
            sync(&path, csoff)?;
        }
        self.config.rows += pending.rows;
        self.config.max_row_size = self.config.max_row_size.max(pending.max_row_size);
        self.config.header = pending.header.clone();
        self.write_config()
    }

    /// Keeps the first `rows` rows and `csoff_len` `.csoff` records, sets `header`, and commits.
    fn truncate(&mut self, rows: u64, csoff_len: u64, header: SegmentHeader) -> io::Result<()> {
        assert!(rows <= self.config.rows);
        let columns = self.config.header.segment.columns();
        let data_end = read_u64_at(&self.offsets, 1 + OFFSET_SIZE * rows * columns)?;
        self.data.set_len(data_end)?;
        self.offsets.set_len(1 + OFFSET_SIZE * (rows * columns + 1))?;
        sync(&self.path, &self.data)?;
        sync(&with_suffix(&self.path, ".off"), &self.offsets)?;
        if let Some(csoff) = &self.csoff {
            csoff.set_len(csoff_len * CSOFF_SIZE)?;
            sync(&with_suffix(&self.path, ".csoff"), csoff)?;
        }
        self.config.rows = rows;
        if rows == 0 {
            self.config.max_row_size = 0;
        }
        self.config.header = header;
        self.config.header.changeset_offsets_len = csoff_len;
        self.write_config()
    }

    fn delete(self) -> io::Result<()> {
        for suffix in [".csoff", ".off", ".conf", ""] {
            match fs::remove_file(with_suffix(&self.path, suffix)) {
                Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
                _ => {}
            }
        }
        Ok(())
    }
}

/// Uncommitted rows for one jar.
#[derive(Debug)]
struct Pending {
    /// The jar's header after these rows.
    header: SegmentHeader,
    /// Value bytes as written (compressed for headers).
    data: Vec<u8>,
    /// End offset of every new value, relative to the committed data end.
    ends: Vec<u64>,
    rows: u64,
    max_row_size: u64,
    /// `(first row relative to the committed rows, count)` per new block.
    csoff: Vec<(u64, u64)>,
}

/// One segment's jars, oldest first, and its uncommitted rows.
#[derive(Debug)]
struct SegmentFiles {
    segment: Segment,
    jars: Vec<Jar>,
    /// Pending rows for the last jar, then for new jars after it.
    pending: Vec<Pending>,
}

impl SegmentFiles {
    /// The header the next append extends: the last pending one, else the last jar's.
    fn tip_header(&self) -> Option<&SegmentHeader> {
        self.pending.last().map(|p| &p.header).or_else(|| self.jars.last().map(|j| &j.config.header))
    }

    /// The next block this segment expects, `None` when it is empty (any first block).
    fn next_block(&self) -> Option<u64> {
        self.tip_header().map(|h| h.blocks.map_or(h.expected.start, |b| b.end + 1))
    }

    /// The next transaction number, from the newest header that has transactions.
    fn next_tx(&self) -> u64 {
        let pending = self.pending.iter().rev().map(|p| &p.header);
        let jars = self.jars.iter().rev().map(|j| &j.config.header);
        pending.chain(jars).find_map(|h| h.txs.map(|t| t.end + 1)).unwrap_or(0)
    }
}

/// The datadir's `static_files/` directory.
#[derive(Debug)]
pub struct StaticFiles {
    dir: PathBuf,
    blocks_per_file: u64,
    segments: RwLock<Vec<SegmentFiles>>,
}

impl StaticFiles {
    /// Opens `dir`, healing every jar. New files use the file size of existing ones, else
    /// [`BLOCKS_PER_FILE`].
    pub fn open(dir: &Path) -> io::Result<Self> {
        Fault::init();
        fs::create_dir_all(dir)?;
        let mut found: Vec<(Segment, Range, PathBuf)> = Vec::new();
        for entry in fs::read_dir(dir)? {
            let name = entry?.file_name();
            let Some(name) = name.to_str() else { continue };
            let mut parts = name.split('_');
            if parts.next() != Some("static") || parts.next() != Some("file") {
                continue;
            }
            let (Some(segment), Some(start), Some(end), None) = (
                parts.next().and_then(Segment::from_name),
                parts.next().and_then(|p| p.parse::<u64>().ok()),
                parts.next().and_then(|p| p.parse::<u64>().ok()),
                parts.next(),
            ) else {
                continue;
            };
            if start <= end {
                found.push((segment, Range::new(start, end), dir.join(name)));
            }
        }
        found.sort_by_key(|(s, r, _)| (*s, r.start));
        let blocks_per_file = found.first().map_or(BLOCKS_PER_FILE, |(_, r, _)| r.len());
        let mut segments = Vec::with_capacity(6);
        for segment in Segment::ALL {
            let mut jars = Vec::new();
            for (_, range, path) in found.iter().filter(|(s, _, _)| *s == segment) {
                assert_eq!(range.len(), blocks_per_file, "mixed static file sizes are not supported");
                if !with_suffix(path, ".conf").exists() {
                    continue;
                }
                jars.push(Jar::open(path.clone())?);
            }
            segments.push(SegmentFiles { segment, jars, pending: Vec::new() });
        }
        Ok(Self { dir: dir.to_path_buf(), blocks_per_file, segments: RwLock::new(segments) })
    }

    /// Drops the in-memory state and opens the files again, as a restart would (after a failed
    /// commit left the files and that state apart).
    pub fn reopen(&self) -> io::Result<()> {
        let fresh = Self::open(&self.dir)?;
        *self.segments.write().unwrap() = fresh.segments.into_inner().unwrap();
        Ok(())
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The committed last block of `segment`, `None` when it has none.
    pub fn last_block(&self, segment: Segment) -> Option<u64> {
        let segments = self.segments.read().unwrap();
        segments[segment as usize].jars.iter().rev().find_map(|j| j.config.header.blocks.map(|b| b.end))
    }

    /// The next block `segment` takes, counting uncommitted rows; `None` when it is empty.
    pub fn next_block(&self, segment: Segment) -> Option<u64> {
        self.segments.read().unwrap()[segment as usize].next_block()
    }

    /// The next transaction number of a transaction segment, counting uncommitted rows.
    pub fn pending_next_tx(&self, segment: Segment) -> u64 {
        assert!(segment.is_tx_based());
        self.segments.read().unwrap()[segment as usize].next_tx()
    }

    /// The committed first block of `segment`, `None` when it has none.
    pub fn first_block(&self, segment: Segment) -> Option<u64> {
        let segments = self.segments.read().unwrap();
        segments[segment as usize].jars.iter().find_map(|j| j.config.header.blocks.map(|b| b.start))
    }

    /// The committed next transaction number of a transaction segment.
    pub fn next_tx(&self, segment: Segment) -> u64 {
        assert!(segment.is_tx_based());
        let segments = self.segments.read().unwrap();
        let files = &segments[segment as usize];
        files.jars.iter().rev().find_map(|j| j.config.header.txs.map(|t| t.end + 1)).unwrap_or(0)
    }

    /// Starts block `block` in `segment` (it must be the next block), rolling to a new file at a
    /// file boundary.
    fn begin_block(&self, files: &mut SegmentFiles, block: u64) -> io::Result<()> {
        if let Some(next) = files.next_block()
            && block != next
        {
            return Err(invalid(format!("{}: appending block {block}, expected {next}", files.segment.name())));
        }
        let tip = files.tip_header().cloned();
        let needs_new_file = match &tip {
            None => true,
            Some(h) => block > h.expected.end,
        };
        if needs_new_file {
            let mut expected = Range::fixed(block, self.blocks_per_file);
            // A segment's first block may lie above its file's start (a genesis above block
            // zero). reth then moves a changeset file's expected start to that block
            // (`set_expected_block_start`) and keeps the other segments' fixed range.
            if tip.is_none() && files.segment.is_change_based() {
                expected.start = block;
            }
            let mut header = JarConfig::new(files.segment, expected).header;
            header.blocks = None;
            files.pending.push(Pending { header, data: Vec::new(), ends: Vec::new(), rows: 0, max_row_size: 0, csoff: Vec::new() });
        } else if files.pending.is_empty() {
            let header = tip.expect("tip");
            files.pending.push(Pending { header, data: Vec::new(), ends: Vec::new(), rows: 0, max_row_size: 0, csoff: Vec::new() });
        }
        let pending = files.pending.last_mut().expect("pending");
        let h = &mut pending.header;
        h.blocks = Some(match h.blocks {
            None => Range::new(block, block),
            Some(b) => Range::new(b.start, block),
        });
        assert!(h.expected.contains(block));
        if files.segment.is_change_based() {
            h.changeset_offsets_len += 1;
            pending.csoff.push((pending.rows, 0));
        }
        Ok(())
    }

    fn push_row(pending: &mut Pending, lz4: bool, columns: &[&[u8]]) {
        let mut size = 0;
        for value in columns {
            size += value.len() as u64;
            if lz4 {
                Lz4::compress(value, &mut pending.data);
            } else {
                pending.data.extend_from_slice(value);
            }
            pending.ends.push(pending.data.len() as u64);
        }
        pending.rows += 1;
        pending.max_row_size = pending.max_row_size.max(size);
    }

    /// Appends block `block`'s header row: the compact header, the compact total difficulty and
    /// the hash.
    pub fn append_header(&self, block: u64, header: &[u8], difficulty: &[u8], hash: &[u8; 32]) -> io::Result<()> {
        let mut segments = self.segments.write().unwrap();
        let files = &mut segments[Segment::Headers as usize];
        self.begin_block(files, block)?;
        let pending = files.pending.last_mut().expect("pending");
        Self::push_row(pending, true, &[header, difficulty, hash]);
        Ok(())
    }

    /// Appends block `block`'s rows to a transaction segment; the first row gets transaction
    /// number `first_tx`, which must be the segment's next.
    pub fn append_txs(&self, segment: Segment, block: u64, first_tx: u64, rows: &[Vec<u8>]) -> io::Result<()> {
        assert!(segment.is_tx_based());
        let mut segments = self.segments.write().unwrap();
        let files = &mut segments[segment as usize];
        let next_tx = files.next_tx();
        if first_tx != next_tx {
            return Err(invalid(format!("{}: block {block} starts at tx {first_tx}, expected {next_tx}", segment.name())));
        }
        self.begin_block(files, block)?;
        let pending = files.pending.last_mut().expect("pending");
        for (i, row) in rows.iter().enumerate() {
            let tx = first_tx + i as u64;
            pending.header.txs = Some(match pending.header.txs {
                None => Range::new(tx, tx),
                Some(t) => Range::new(t.start, tx),
            });
            Self::push_row(pending, false, &[row]);
        }
        Ok(())
    }

    /// Appends block `block`'s changeset rows (already sorted as reth sorts them).
    pub fn append_changeset(&self, segment: Segment, block: u64, rows: &[Vec<u8>]) -> io::Result<()> {
        assert!(segment.is_change_based());
        let mut segments = self.segments.write().unwrap();
        let files = &mut segments[segment as usize];
        self.begin_block(files, block)?;
        let pending = files.pending.last_mut().expect("pending");
        for row in rows {
            Self::push_row(pending, false, &[row]);
        }
        pending.csoff.last_mut().expect("block begun").1 = rows.len() as u64;
        Ok(())
    }

    /// Whether any segment has uncommitted rows.
    pub fn is_dirty(&self) -> bool {
        self.segments.read().unwrap().iter().any(|f| !f.pending.is_empty())
    }

    /// Commits every pending row, segment by segment.
    pub fn commit(&self) -> io::Result<()> {
        let mut segments = self.segments.write().unwrap();
        for files in segments.iter_mut() {
            let pending = std::mem::take(&mut files.pending);
            for p in pending {
                let is_last = files.jars.last().is_some_and(|j| j.config.header.expected == p.header.expected);
                if !is_last {
                    // Files are named after their fixed range even when the expected start moved.
                    let path = self.dir.join(files.segment.file_name(Range::fixed(p.header.expected.start, self.blocks_per_file)));
                    let mut config = JarConfig::new(files.segment, p.header.expected);
                    // The block range is set by the append; an empty new file has none.
                    config.header.blocks = None;
                    files.jars.push(Jar::create(path, config)?);
                }
                files.jars.last_mut().expect("jar").append(&p)?;
            }
        }
        Ok(())
    }

    /// Drops uncommitted rows.
    pub fn discard(&self) {
        for files in self.segments.write().unwrap().iter_mut() {
            files.pending.clear();
        }
    }

    /// Removes every committed block after `last_block` from `segment`; for transaction
    /// segments, `next_tx` is the first transaction number to remove. Commits at once.
    pub fn truncate(&self, segment: Segment, last_block: u64, next_tx: Option<u64>) -> io::Result<()> {
        assert_eq!(segment.is_tx_based(), next_tx.is_some());
        let mut segments = self.segments.write().unwrap();
        let files = &mut segments[segment as usize];
        assert!(files.pending.is_empty(), "truncate with pending rows");
        // Delete whole files after the one holding `last_block` (keeping the first file).
        while files.jars.len() > 1 && files.jars.last().expect("jar").config.header.expected.start > last_block {
            files.jars.pop().expect("jar").delete()?;
        }
        let Some(jar) = files.jars.last_mut() else { return Ok(()) };
        let mut header = jar.config.header.clone();
        let Some(blocks) = header.blocks else { return Ok(()) };
        if blocks.end <= last_block {
            return Ok(());
        }
        let keep_blocks = if last_block >= blocks.start { last_block - blocks.start + 1 } else { 0 };
        header.blocks = (keep_blocks > 0).then(|| Range::new(blocks.start, last_block));
        let (rows, csoff_len) = if segment.is_change_based() {
            let len = keep_blocks.min(header.changeset_offsets_len);
            let rows = if len < header.changeset_offsets_len {
                jar.changeset(blocks.start + len)?.expect("record").0
            } else {
                jar.config.rows
            };
            (rows, len)
        } else if segment.is_tx_based() {
            let next_tx = next_tx.expect("tx segment");
            let rows = match header.txs {
                Some(t) if next_tx > t.start => (next_tx - t.start).min(t.len()),
                _ => 0,
            };
            header.txs = header.txs.and_then(|t| (rows > 0).then(|| Range::new(t.start, t.start + rows - 1)));
            // A file whose blocks are all gone keeps its block range only when it is the first.
            (rows, 0)
        } else {
            (keep_blocks, 0)
        };
        jar.truncate(rows, csoff_len, header)
    }

    /// Deletes every jar of `segment`.
    pub fn clear(&self, segment: Segment) -> io::Result<()> {
        let mut segments = self.segments.write().unwrap();
        let files = &mut segments[segment as usize];
        files.pending.clear();
        for jar in files.jars.drain(..) {
            jar.delete()?;
        }
        Ok(())
    }

    /// Runs `f` on the committed jar of `segment` holding block `block`.
    pub fn with_block_jar<T>(&self, segment: Segment, block: u64, f: impl FnOnce(&Jar) -> io::Result<T>) -> io::Result<Option<T>> {
        let segments = self.segments.read().unwrap();
        let jars = &segments[segment as usize].jars;
        match jars.iter().find(|j| j.config.header.blocks.is_some_and(|b| b.contains(block))) {
            Some(jar) => f(jar).map(Some),
            None => Ok(None),
        }
    }

    /// Runs `f` on the committed jar of a transaction segment holding transaction `tx`, with the
    /// row of `tx` in it.
    pub fn with_tx_jar<T>(&self, segment: Segment, tx: u64, f: impl FnOnce(&Jar, u64) -> io::Result<T>) -> io::Result<Option<T>> {
        let segments = self.segments.read().unwrap();
        let jars = &segments[segment as usize].jars;
        match jars.iter().find(|j| j.config.header.txs.is_some_and(|t| t.contains(tx))) {
            Some(jar) => f(jar, tx - jar.config.header.txs.expect("txs").start).map(Some),
            None => Ok(None),
        }
    }

    /// Block `block`'s header columns, or `None` when not stored.
    pub fn header(&self, block: u64) -> io::Result<Option<[Vec<u8>; 3]>> {
        self.with_block_jar(Segment::Headers, block, |jar| {
            let row = block - jar.config.header.blocks.expect("blocks").start;
            let mut columns = [Vec::new(), Vec::new(), Vec::new()];
            for (c, out) in columns.iter_mut().enumerate() {
                jar.value(row, c as u64, out)?;
            }
            Ok(columns)
        })
    }

    /// Transaction `tx`'s row of a transaction segment.
    pub fn tx_row(&self, segment: Segment, tx: u64) -> io::Result<Option<Vec<u8>>> {
        self.with_tx_jar(segment, tx, |jar, row| {
            let mut out = Vec::new();
            jar.value(row, 0, &mut out)?;
            Ok(out)
        })
    }

    /// Block `block`'s changeset rows, or `None` when the block is not stored.
    pub fn changeset(&self, segment: Segment, block: u64) -> io::Result<Option<Vec<Vec<u8>>>> {
        assert!(segment.is_change_based());
        let rows = self.with_block_jar(segment, block, |jar| {
            let Some((first, count)) = jar.changeset(block)? else { return Ok(None) };
            let mut rows = Vec::with_capacity(count as usize);
            for row in first..first + count {
                let mut out = Vec::new();
                jar.value(row, 0, &mut out)?;
                rows.push(out);
            }
            Ok(Some(rows))
        })?;
        Ok(rows.flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tiger-sf-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn lz4_literal_blocks_round_trip() {
        for len in [0usize, 1, 14, 15, 16, 269, 270, 271, 1000] {
            let src: Vec<u8> = (0..len).map(|i| (i * 7) as u8).collect();
            let mut enc = Vec::new();
            Lz4::compress(&src, &mut enc);
            let mut dec = Vec::new();
            Lz4::decompress(&enc, len, &mut dec).unwrap();
            assert_eq!(dec, src);
        }
        // A block with a match: literals "abcd", then copy offset 4 length 8 -> "abcdabcdabcd".
        let block = [0x44, b'a', b'b', b'c', b'd', 4, 0, 0x10, b'z'];
        let mut dec = Vec::new();
        // Token 0x44: 4 literals, match length 4+4=8; then a final literal-only sequence "z".
        Lz4::decompress(&block, 100, &mut dec).unwrap();
        assert_eq!(dec, b"abcdabcdabcdz");
        assert!(Lz4::decompress(&block, 5, &mut Vec::new()).is_err());
    }

    #[test]
    fn config_round_trips() {
        let mut config = JarConfig::new(Segment::StorageChangeSets, Range::new(0, 499_999));
        config.header.blocks = Some(Range::new(0, 19));
        config.header.changeset_offsets_len = 20;
        config.rows = 0x33fe;
        config.max_row_size = 0x54;
        let bytes = config.encode();
        assert_eq!(bytes.len(), 79);
        assert_eq!(JarConfig::decode(&bytes).unwrap(), config);
        let headers = JarConfig::new(Segment::Headers, Range::new(0, 499_999));
        assert_eq!(JarConfig::decode(&headers.encode()).unwrap(), headers);
    }

    #[test]
    fn append_read_truncate_reopen() {
        let dir = tmp("basic");
        let sf = StaticFiles::open(&dir).unwrap();
        let mut next_tx = 0;
        for block in 0..30u64 {
            sf.append_header(block, &[block as u8; 40], &[], &[block as u8; 32]).unwrap();
            let rows: Vec<Vec<u8>> = (0..block % 3).map(|i| vec![block as u8, i as u8]).collect();
            sf.append_txs(Segment::Transactions, block, next_tx, &rows).unwrap();
            next_tx += rows.len() as u64;
            let changes: Vec<Vec<u8>> = (0..block % 4).map(|i| vec![i as u8; 21]).collect();
            sf.append_changeset(Segment::AccountChangeSets, block, &changes).unwrap();
            if block % 7 == 6 {
                sf.commit().unwrap();
            }
        }
        sf.commit().unwrap();
        drop(sf);
        let sf = StaticFiles::open(&dir).unwrap();
        assert_eq!(sf.last_block(Segment::Headers), Some(29));
        assert_eq!(sf.header(5).unwrap().unwrap(), [vec![5; 40], vec![], vec![5; 32]]);
        assert_eq!(sf.changeset(Segment::AccountChangeSets, 7).unwrap().unwrap(), vec![vec![0; 21], vec![1; 21], vec![2; 21]]);
        assert_eq!(sf.changeset(Segment::AccountChangeSets, 8).unwrap().unwrap(), Vec::<Vec<u8>>::new());
        assert_eq!(sf.next_tx(Segment::Transactions), next_tx);
        // Unwind to block 20.
        let txs_through_20: u64 = (0..=20u64).map(|b| b % 3).sum();
        sf.truncate(Segment::Headers, 20, None).unwrap();
        sf.truncate(Segment::Transactions, 20, Some(txs_through_20)).unwrap();
        sf.truncate(Segment::AccountChangeSets, 20, None).unwrap();
        drop(sf);
        let sf = StaticFiles::open(&dir).unwrap();
        assert_eq!(sf.last_block(Segment::Headers), Some(20));
        assert_eq!(sf.last_block(Segment::AccountChangeSets), Some(20));
        assert_eq!(sf.next_tx(Segment::Transactions), txs_through_20);
        assert!(sf.header(21).unwrap().is_none());
        assert_eq!(sf.changeset(Segment::AccountChangeSets, 19).unwrap().unwrap().len(), 3);
        // Append again after the unwind.
        sf.append_header(21, &[9; 3], &[], &[9; 32]).unwrap();
        sf.append_changeset(Segment::AccountChangeSets, 21, &[vec![7; 21]]).unwrap();
        sf.commit().unwrap();
        assert_eq!(sf.changeset(Segment::AccountChangeSets, 21).unwrap().unwrap(), vec![vec![7; 21]]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rolls_over_file_boundaries_and_unwinds_across_them() {
        let dir = tmp("roll");
        fs::create_dir_all(&dir).unwrap();
        // Force 10-block files by seeding an empty jar of that size.
        let first = dir.join(Segment::Headers.file_name(Range::new(0, 9)));
        Jar::create(first, JarConfig::new(Segment::Headers, Range::new(0, 9))).unwrap();
        let sf = StaticFiles::open(&dir).unwrap();
        for block in 0..25u64 {
            sf.append_header(block, &[block as u8; 5], &[], &[0; 32]).unwrap();
            sf.append_changeset(Segment::StorageChangeSets, block, &[vec![block as u8; 53]]).unwrap();
        }
        sf.commit().unwrap();
        assert!(dir.join("static_file_headers_20_29.conf").exists());
        assert!(dir.join("static_file_storage-change-sets_10_19.csoff").exists());
        assert_eq!(sf.header(24).unwrap().unwrap()[0], vec![24; 5]);
        assert_eq!(sf.changeset(Segment::StorageChangeSets, 15).unwrap().unwrap(), vec![vec![15; 53]]);
        sf.truncate(Segment::Headers, 8, None).unwrap();
        sf.truncate(Segment::StorageChangeSets, 8, None).unwrap();
        assert!(!dir.join("static_file_headers_20_29.conf").exists());
        assert!(!dir.join("static_file_headers_10_19").exists());
        drop(sf);
        let sf = StaticFiles::open(&dir).unwrap();
        assert_eq!(sf.last_block(Segment::Headers), Some(8));
        sf.append_header(9, &[1], &[], &[0; 32]).unwrap();
        sf.append_header(10, &[2], &[], &[0; 32]).unwrap();
        sf.commit().unwrap();
        assert_eq!(sf.header(10).unwrap().unwrap()[0], vec![2]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn heals_uncommitted_tails() {
        let dir = tmp("heal");
        let sf = StaticFiles::open(&dir).unwrap();
        for block in 0..5u64 {
            sf.append_changeset(Segment::AccountChangeSets, block, &[vec![block as u8; 30], vec![1; 30]]).unwrap();
        }
        sf.commit().unwrap();
        drop(sf);
        let path = dir.join(Segment::AccountChangeSets.file_name(Range::new(0, BLOCKS_PER_FILE - 1)));
        let conf = fs::read(with_suffix(&path, ".conf")).unwrap();
        let (data, off, csoff) =
            (fs::read(&path).unwrap(), fs::read(with_suffix(&path, ".off")).unwrap(), fs::read(with_suffix(&path, ".csoff")).unwrap());
        // A crash after data, offsets and csoff were appended but before the config was replaced.
        let sf = StaticFiles::open(&dir).unwrap();
        sf.append_changeset(Segment::AccountChangeSets, 5, &[vec![5; 30]]).unwrap();
        sf.commit().unwrap();
        drop(sf);
        fs::write(with_suffix(&path, ".conf"), &conf).unwrap();
        let sf = StaticFiles::open(&dir).unwrap();
        assert_eq!(sf.last_block(Segment::AccountChangeSets), Some(4));
        drop(sf);
        assert_eq!(fs::read(&path).unwrap(), data);
        assert_eq!(fs::read(with_suffix(&path, ".off")).unwrap(), off);
        assert_eq!(fs::read(with_suffix(&path, ".csoff")).unwrap(), csoff);
        // A config that counts rows whose offsets were lost: rows and blocks are lowered.
        fs::write(with_suffix(&path, ".off"), &off[..off.len() - 16]).unwrap();
        let sf = StaticFiles::open(&dir).unwrap();
        assert_eq!(sf.last_block(Segment::AccountChangeSets), Some(3));
        assert_eq!(sf.changeset(Segment::AccountChangeSets, 3).unwrap().unwrap().len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Reads and re-encodes a base-reth datadir's static files (set `TIGER_RETH_DATADIR`).
    #[test]
    fn reads_a_reth_datadir() {
        let Ok(datadir) = std::env::var("TIGER_RETH_DATADIR") else { return };
        let dir = Path::new(&datadir).join("static_files");
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "conf") {
                let bytes = fs::read(&path).unwrap();
                assert_eq!(JarConfig::decode(&bytes).unwrap().encode(), bytes, "{}", path.display());
            }
        }
        let sf = StaticFiles::open(&dir).unwrap();
        let last = sf.last_block(Segment::Headers).unwrap();
        for block in 0..=last {
            let [header, _, hash] = sf.header(block).unwrap().unwrap();
            let decoded = crate::HeaderCodec::decode(&header).unwrap();
            assert_eq!(decoded.number, block);
            assert_eq!(crate::HeaderCodec::encode(&decoded), header, "header {block} re-encodes");
            assert_eq!(decoded.hash_slow().as_slice(), &hash[..], "header {block} hash");
        }
        let changes = sf.changeset(Segment::AccountChangeSets, 0).unwrap().unwrap();
        assert!(!changes.is_empty(), "genesis allocations are block 0 changes");
    }
}
