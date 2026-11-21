//! The durable storage seam: an append-only record log in one folder, with fsync and crash recovery.
//!
//! pe-agree's safety data (agreement.md 8's vote state; F3's fence, pending branch data and tail witnesses) and the
//! S5 overflow journal are written through it. The log knows nothing of their meaning. A record is `(kind, bytes)`:
//! each user owns its kind numbers (below [`RESERVED_KINDS`]) and its codec.
//!
//! # Guarantees
//!
//! - [`Log::append`] only buffers. [`Log::sync`] writes the buffered records and fsyncs once (a group commit), and
//!   when it returns `Ok(lsn)`, every record through `lsn` survives a crash.
//! - Reopening after a crash yields exactly the records of some point between the last successful sync and the last
//!   append, in order: never a record that wasn't appended, never one out of order, never one lost that a sync
//!   acknowledged. A write the crash cut short or scrambled (an unsynced write can reach the disk in any order) is
//!   dropped and the file truncated.
//! - Damage the log can prove hit synced data is [`StoreError::Corrupt`], never skipped: the caller must not start on
//!   it (@world-chunks: no vote on guessed state). The proof is a commit marker: each batch begins with
//!   one that vouches for everything before it, which was synced before the marker was written. So damage followed
//!   by a valid marker is corruption, and damage with no marker after it lies in the last, unacknowledged batch.
//!   The one gap: damage to the last synced batch before any later write reads as a torn write. Opening the log
//!   writes a marker at once, so that window closes at the next open.
//! - [`Log::checkpoint`] replaces the whole log with a live set, atomically: after a crash the log holds either the
//!   old records or exactly the live set. No step relies on rename being atomic or on a directory fsync, which
//!   Windows lacks. A checkpoint writes a new, higher segment, which counts only once the marker after its end is
//!   durable, and only then deletes the old one.
//! - After a failed write or fsync the log is poisoned: the state of the file is unknown (an fsync error may have
//!   dropped dirty pages), so every later call fails until the log is reopened, which recovers. The one exception
//!   is a failed write (a full disk, say) whose bytes could be cut back off: the file is as it was, the records stay
//!   buffered, and a later [`Log::sync`] retries.
//!
//! # Kinds
//!
//! Kinds 1-15 are pe-agree's safety records (vote state, the fence, pending branch data, tail witnesses), 16 up to
//! [`RESERVED_KINDS`] the overflow journal's (@world-chunks). Each piece owns its numbers' codecs.
//!
//! # Use from a node
//!
//! Node stays sans-IO: it emits numbered [`Job`]s in order and holds every message that depends on one (a vote, a
//! timeout, the `Branch` after the fence, a journal ack) until the driver reports that job durable. [`persist`] runs
//! a batch inline (tests, simple drivers); [`Writer`] runs the log on its own thread for pe-app, so no fsync lands
//! on the frame loop, and commits everything queued with one fsync.
//!
//! # Format
//!
//! The folder holds segments `<16 hex digits>.seg`. A segment is a header (magic, segment number, the Lsn of its
//! first frame, a random salt, a check), then frames: the checkpoint's live records, an end marker, a commit marker,
//! then batches of appended records, each after a commit marker. A frame is `len u32 | kind u16 | check [16] |
//! payload`, little-endian. The check is 16 bytes of blake3 over the salt, the segment number, the frame's Lsn,
//! `len`, `kind` and the payload: a stale frame left in a reused file can't pass for the next one, and a peer's
//! bytes inside a journalled entry can't pass for a marker, since the salt never leaves the machine. Lsns count
//! frames, markers included, and only grow.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

/// A record's place in the log. Lsns only grow, across checkpoints too. `Lsn(0)` is "nothing".
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Lsn(pub u64);

/// Kinds from here up are the log's own.
pub const RESERVED_KINDS: u16 = 0xFF00;
/// The end of a checkpoint's live set. Payload: its own Lsn, then the number of live records.
const KIND_END: u16 = 0xFFFF;
/// Everything before this frame was synced before it was written. Payload: its own Lsn.
const KIND_COMMIT: u16 = 0xFFFE;

const MAGIC: &[u8; 8] = b"PEAGLOG1";
const HEADER: usize = 8 + 8 + 8 + 16 + 16;
const FRAME: usize = 4 + 2 + 16;
const DOMAIN: &[u8] = b"pe-agree store v1 frame";
const HEADER_DOMAIN: &[u8] = b"pe-agree store v1 header";

/// A recovered record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub lsn: Lsn,
    pub kind: u16,
    pub payload: Vec<u8>,
}

/// What [`Log::open`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recovered {
    /// The live records, in order: the last checkpoint's, then those appended after it.
    pub records: Vec<Record>,
    /// Bytes of a torn last write that were cut off (0 if none).
    pub torn_bytes: u64,
    /// Segments left by an interrupted checkpoint, or made obsolete by a finished one, that were removed.
    pub removed_segments: usize,
}

#[derive(Debug)]
pub enum StoreError {
    /// The disk is full. If the log isn't poisoned, the records stay buffered and a later sync retries.
    Full,
    /// Damage to data that was synced (see the module documentation). Don't start on it.
    Corrupt {
        segment: u64,
        offset: u64,
        why: &'static str,
    },
    /// A record over [`Options::max_record`], a kind at or above [`RESERVED_KINDS`], or a job out of order.
    Refused(&'static str),
    /// An earlier write or fsync failed: reopen the log.
    Poisoned,
    Io(io::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Full => write!(f, "the disk is full"),
            StoreError::Corrupt { segment, offset, why } => {
                write!(f, "segment {segment:016x} is corrupt at byte {offset}: {why}")
            }
            StoreError::Refused(why) => write!(f, "refused: {why}"),
            StoreError::Poisoned => write!(f, "an earlier write or fsync failed; reopen the log"),
            StoreError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StoreError {}

fn io_err(e: io::Error) -> StoreError {
    if e.kind() == io::ErrorKind::StorageFull { StoreError::Full } else { StoreError::Io(e) }
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// The largest payload accepted, and believed when recovering (a larger length is a torn or corrupt frame).
    pub max_record: u32,
}

impl Default for Options {
    fn default() -> Options {
        Options { max_record: 16 << 20 }
    }
}

/// The file operations the log needs. [`StdFs`] is the real one; `SimFs` (tests, `test-util`) models a page cache
/// and crashes.
pub trait Fs {
    type File;
    fn create_dir_all(&mut self, dir: &Path) -> io::Result<()>;
    /// The names of the files in `dir`.
    fn list(&mut self, dir: &Path) -> io::Result<Vec<String>>;
    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>>;
    /// `len` bytes of the file from `offset` (fewer at its end).
    fn read_at(&mut self, path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>>;
    /// Create a new, empty file; an existing one is an error.
    fn create_new(&mut self, path: &Path) -> io::Result<Self::File>;
    /// Open an existing file to append to.
    fn open_append(&mut self, path: &Path) -> io::Result<Self::File>;
    fn append(&mut self, file: &mut Self::File, bytes: &[u8]) -> io::Result<()>;
    /// Make the file's bytes (and its own existence, where the platform ties them) durable.
    fn sync(&mut self, file: &mut Self::File) -> io::Result<()>;
    /// Cut the file to `len` bytes, durably.
    fn truncate(&mut self, file: &mut Self::File, len: u64) -> io::Result<()>;
    fn remove(&mut self, path: &Path) -> io::Result<()>;
    /// Make the folder's entries (created and removed files) durable. A no-op where the platform can't.
    fn sync_dir(&mut self, dir: &Path) -> io::Result<()>;
    /// 16 bytes no peer can predict, for a new segment's salt.
    fn salt(&mut self) -> [u8; 16];
}

/// The real file system.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdFs;

impl Fs for StdFs {
    type File = std::fs::File;

    fn create_dir_all(&mut self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)
    }

    fn list(&mut self, dir: &Path) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            if e.file_type()?.is_file()
                && let Some(n) = e.file_name().to_str()
            {
                names.push(n.to_owned());
            }
        }
        Ok(names)
    }

    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn read_at(&mut self, path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        use std::io::{Read, Seek};
        let mut f = std::fs::File::open(path)?;
        f.seek(io::SeekFrom::Start(offset))?;
        let mut out = Vec::with_capacity(len);
        f.take(len as u64).read_to_end(&mut out)?;
        Ok(out)
    }

    fn create_new(&mut self, path: &Path) -> io::Result<std::fs::File> {
        std::fs::OpenOptions::new().append(true).create_new(true).open(path)
    }

    fn open_append(&mut self, path: &Path) -> io::Result<std::fs::File> {
        std::fs::OpenOptions::new().append(true).open(path)
    }

    fn append(&mut self, file: &mut std::fs::File, bytes: &[u8]) -> io::Result<()> {
        io::Write::write_all(file, bytes)
    }

    fn sync(&mut self, file: &mut std::fs::File) -> io::Result<()> {
        // FlushFileBuffers on Windows, which also flushes the file's metadata (NTFS journals its creation);
        // fdatasync elsewhere, with the folder synced separately.
        file.sync_data()
    }

    fn truncate(&mut self, file: &mut std::fs::File, len: u64) -> io::Result<()> {
        file.set_len(len)?;
        file.sync_all()
    }

    fn remove(&mut self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    #[cfg(unix)]
    fn sync_dir(&mut self, dir: &Path) -> io::Result<()> {
        std::fs::File::open(dir)?.sync_all()
    }

    #[cfg(not(unix))]
    fn sync_dir(&mut self, _dir: &Path) -> io::Result<()> {
        Ok(())
    }

    fn salt(&mut self) -> [u8; 16] {
        // std's hasher keys come from the OS's randomness; two of them, mixed with the time, give 16 bytes that a
        // remote peer can't predict. (Not a key: it only keeps a peer's bytes from passing for a marker.)
        use std::hash::{BuildHasher, Hasher};
        let mut out = [0u8; 16];
        for (i, half) in out.chunks_mut(8).enumerate() {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_usize(i);
            h.write_u128(
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()),
            );
            half.copy_from_slice(&h.finish().to_le_bytes());
        }
        out
    }
}

/// A segment's identity in its frames' checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Seg {
    pub(crate) n: u64,
    pub(crate) salt: [u8; 16],
}

pub(crate) fn check(seg: Seg, lsn: u64, kind: u16, payload: &[u8]) -> [u8; 16] {
    let mut h = blake3::Hasher::new();
    h.update(DOMAIN);
    h.update(&seg.salt);
    h.update(&seg.n.to_le_bytes());
    h.update(&lsn.to_le_bytes());
    h.update(&(payload.len() as u32).to_le_bytes());
    h.update(&kind.to_le_bytes());
    h.update(payload);
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize().as_bytes()[..16]);
    out
}

fn header(seg: Seg, base: u64) -> Vec<u8> {
    let mut b = Vec::with_capacity(HEADER);
    b.extend_from_slice(MAGIC);
    b.extend_from_slice(&seg.n.to_le_bytes());
    b.extend_from_slice(&base.to_le_bytes());
    b.extend_from_slice(&seg.salt);
    let mut h = blake3::Hasher::new();
    h.update(HEADER_DOMAIN);
    h.update(&b);
    b.extend_from_slice(&h.finalize().as_bytes()[..16]);
    b
}

fn frame(out: &mut Vec<u8>, seg: Seg, lsn: u64, kind: u16, payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&check(seg, lsn, kind, payload));
    out.extend_from_slice(payload);
}

fn commit_frame(out: &mut Vec<u8>, seg: Seg, lsn: u64) {
    frame(out, seg, lsn, KIND_COMMIT, &lsn.to_le_bytes());
}

fn seg_name(n: u64) -> String {
    format!("{n:016x}.seg")
}

fn parse_name(name: &str) -> Option<u64> {
    let hex = name.strip_suffix(".seg")?;
    if hex.len() != 16 || !hex.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) {
        return None;
    }
    u64::from_str_radix(hex, 16).ok()
}

fn le_u64(b: &[u8]) -> Option<u64> {
    b.try_into().ok().map(u64::from_le_bytes)
}

/// The valid frame at `at` with Lsn `lsn`: (kind, payload, end).
fn parse_frame(bytes: &[u8], at: usize, seg: Seg, lsn: u64, max: u32) -> Option<(u16, &[u8], usize)> {
    let head = bytes.get(at..at.checked_add(FRAME)?)?;
    let len = u32::from_le_bytes(head[0..4].try_into().ok()?);
    let kind = u16::from_le_bytes(head[4..6].try_into().ok()?);
    if len > max {
        return None;
    }
    let end = at + FRAME + len as usize;
    let payload = bytes.get(at + FRAME..end)?;
    (head[6..22] == check(seg, lsn, kind, payload)).then_some((kind, payload, end))
}

/// Whether a valid commit marker with an Lsn of at least `min` starts anywhere after `at`: proof that the bytes at
/// `at` were synced (see the module documentation). Markers are found by their shape and their own Lsn, so the
/// search needs no frame boundaries.
fn marker_after(bytes: &[u8], at: usize, seg: Seg, min: u64) -> bool {
    let len8 = 8u32.to_le_bytes();
    let kind = KIND_COMMIT.to_le_bytes();
    let mut i = at + 1;
    while i + FRAME + 8 <= bytes.len() {
        if bytes[i..i + 4] == len8 && bytes[i + 4..i + 6] == kind {
            let lsn = le_u64(&bytes[i + FRAME..i + FRAME + 8]).unwrap_or(0);
            if lsn >= min && parse_frame(bytes, i, seg, lsn, 8).is_some() {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// A segment as read back.
#[derive(Debug)]
struct Scan {
    seg: Seg,
    /// The checkpoint's live records and the appended ones after it.
    records: Vec<Record>,
    /// Each record's offset in the file.
    offsets: Vec<u64>,
    /// Through the last valid frame.
    valid_len: u64,
    file_len: u64,
    /// The Lsn the next frame gets.
    next_lsn: u64,
    /// Whether the last valid frame is a commit marker (nothing written since is unvouched).
    vouched: bool,
}

/// The length of the smallest finished segment: a header, an empty live set's end marker and its commit. A torn
/// first write of the log's first segment can't be longer; a later checkpoint's always has a predecessor that counts.
const EMPTY_SEGMENT: usize = HEADER + FRAME + 16 + FRAME + 8;

/// What a segment turned out to be.
#[derive(Debug)]
enum Scanned {
    /// A finished checkpoint, with what was appended after it.
    Counts(Scan),
    /// A checkpoint that never finished: its header is intact (with the Lsn its first frame would get), but no commit
    /// follows its end marker. Safe to remove once a predecessor counts.
    Unfinished { base: u64 },
    /// Too short for a header: a torn first write. Safe to remove.
    Torn,
    /// A header that fails its check, with no sign that the segment ever finished. Never removed: it may be a
    /// finished segment whose salt was damaged, which nothing can prove.
    Unreadable,
}

/// Read a segment, named `n`.
fn scan(bytes: &[u8], n: u64, max: u32) -> Result<Scanned, StoreError> {
    let Some(h) = bytes.get(..HEADER) else { return Ok(Scanned::Torn) };
    let base = le_u64(&h[16..24]).unwrap_or(0);
    let mut salt = [0u8; 16];
    salt.copy_from_slice(&h[24..40]);
    // The name gives the segment number; the salt is taken as written.
    let seg = Seg { n, salt };
    if &h[..8] != MAGIC || h != header(seg, base).as_slice() {
        // A header written whole and then damaged still has its salt, unless the damage hit the salt: a valid commit
        // marker under it proves the segment had finished.
        if marker_after(bytes, HEADER - 1, seg, 0) {
            return Err(StoreError::Corrupt { segment: n, offset: 0, why: "a damaged header on a finished segment" });
        }
        return Ok(Scanned::Unreadable);
    }
    let corrupt = |at: usize, why| StoreError::Corrupt { segment: n, offset: at as u64, why };
    let mut at = HEADER;
    let mut lsn = base;
    let mut records = Vec::new();
    let mut offsets = Vec::new();
    // 0: in the live set; 1: after its end marker, before the checkpoint's commit; 2: finished.
    let mut stage = 0;
    let mut vouched = false;
    while let Some((kind, payload, end)) = parse_frame(bytes, at, seg, lsn, max) {
        match (stage, kind) {
            (0, KIND_END) => {
                if le_u64(payload.get(..8).unwrap_or(&[])) != Some(lsn)
                    || le_u64(payload.get(8..16).unwrap_or(&[])) != Some(records.len() as u64)
                    || payload.len() != 16
                {
                    return Err(corrupt(at, "an end marker that doesn't match its checkpoint"));
                }
                stage = 1;
            }
            (1, KIND_COMMIT) | (2, KIND_COMMIT) => {
                if payload != lsn.to_le_bytes() {
                    return Err(corrupt(at, "a commit marker that names another place"));
                }
                stage = 2;
                vouched = true;
            }
            (0, k) | (2, k) if k < RESERVED_KINDS => {
                records.push(Record { lsn: Lsn(lsn), kind: k, payload: payload.to_vec() });
                offsets.push(at as u64);
                vouched = false;
            }
            _ => return Err(corrupt(at, "a marker out of place")),
        }
        at = end;
        lsn += 1;
    }
    let proven = at < bytes.len() && marker_after(bytes, at, seg, lsn);
    if proven {
        return Err(corrupt(at, "damaged data with a commit marker after it"));
    }
    if stage < 2 {
        // The checkpoint's own commit marker is its last write, synced before it returns.
        return Ok(Scanned::Unfinished { base });
    }
    let scan =
        Scan { seg, records, offsets, valid_len: at as u64, file_len: bytes.len() as u64, next_lsn: lsn, vouched };
    Ok(Scanned::Counts(scan))
}

/// The log. See the module documentation.
pub struct Log<F: Fs> {
    fs: F,
    dir: PathBuf,
    opts: Options,
    seg: Seg,
    /// Above every segment name seen, so a leftover file (a delete that failed) is never reused.
    next_segment: u64,
    file: F::File,
    /// Bytes in the segment file, all synced (unless poisoned).
    written: u64,
    /// Frames buffered by `append`, not yet written.
    pending: Vec<u8>,
    /// Each record's offset in the current segment, by Lsn (markers aren't records).
    index: BTreeMap<u64, u64>,
    /// Whether the next batch must begin with a commit marker (something synced is unvouched).
    owe_commit: bool,
    next_lsn: u64,
    durable: Lsn,
    poisoned: bool,
}

impl<F: Fs> Log<F> {
    /// Open the log in `dir`, creating it if needed, and recover what it holds.
    pub fn open(mut fs: F, dir: &Path, opts: Options) -> Result<(Log<F>, Recovered), StoreError> {
        fs.create_dir_all(dir).map_err(io_err)?;
        let mut segs: Vec<u64> = fs.list(dir).map_err(io_err)?.iter().filter_map(|n| parse_name(n)).collect();
        segs.sort_unstable();
        let highest = segs.last().copied().unwrap_or(0);
        let mut found = None;
        let mut obsolete = Vec::new();
        // Unfinished segments above the one that counts: their bases.
        let mut above: Vec<u64> = Vec::new();
        let mut longest = 0;
        for &s in segs.iter().rev() {
            if found.is_some() {
                obsolete.push(s);
                continue;
            }
            let bytes = fs.read(&dir.join(seg_name(s))).map_err(io_err)?;
            match scan(&bytes, s, opts.max_record)? {
                Scanned::Counts(scan) => {
                    // A checkpoint syncs its predecessor first, so the predecessor of an unfinished checkpoint ends
                    // at or past its base. An older segment (a delete Windows refused) ends before it: falling back
                    // to it would bring back old state, a double vote.
                    if above.iter().any(|b| scan.next_lsn < *b) {
                        let why = "the segment that counts is older than an unfinished checkpoint above it";
                        return Err(StoreError::Corrupt { segment: s, offset: 0, why });
                    }
                    found = Some(scan);
                }
                Scanned::Unfinished { base } => {
                    above.push(base);
                    longest = longest.max(bytes.len());
                    obsolete.push(s);
                }
                Scanned::Torn => obsolete.push(s),
                Scanned::Unreadable => longest = longest.max(bytes.len()),
            }
        }
        if found.is_none() && longest > EMPTY_SEGMENT {
            // Nothing counts, yet a segment holds more than a torn first segment can: a finished one was damaged.
            return Err(StoreError::Corrupt {
                segment: highest,
                offset: 0,
                why: "no segment counts, but one is longer than a first segment cut short",
            });
        }
        let mut recovered = Recovered::default();
        let mut log = match found {
            Some(scan) => {
                let mut file = fs.open_append(&dir.join(seg_name(scan.seg.n))).map_err(io_err)?;
                if scan.valid_len < scan.file_len {
                    fs.truncate(&mut file, scan.valid_len).map_err(io_err)?;
                    recovered.torn_bytes = scan.file_len - scan.valid_len;
                }
                let index = scan.records.iter().zip(&scan.offsets).map(|(r, o)| (r.lsn.0, *o)).collect();
                recovered.records = scan.records;
                Log {
                    fs,
                    dir: dir.to_owned(),
                    opts,
                    seg: scan.seg,
                    next_segment: highest + 1,
                    file,
                    written: scan.valid_len,
                    pending: Vec::new(),
                    index,
                    owe_commit: !scan.vouched,
                    next_lsn: scan.next_lsn,
                    durable: Lsn(scan.next_lsn - 1),
                    poisoned: false,
                }
            }
            None => {
                // Nothing counted: start with an empty checkpoint, above every name present.
                let seg = Seg { n: highest + 1, salt: fs.salt() };
                let (file, len, next_lsn, _) = write_segment(&mut fs, dir, seg, 1, &[]).map_err(SegmentError::error)?;
                Log {
                    fs,
                    dir: dir.to_owned(),
                    opts,
                    seg,
                    next_segment: highest + 2,
                    file,
                    written: len,
                    pending: Vec::new(),
                    index: BTreeMap::new(),
                    owe_commit: false,
                    next_lsn,
                    durable: Lsn(next_lsn - 1),
                    poisoned: false,
                }
            }
        };
        // Vouch for the last synced batch now, so damage to it is told from a torn write from here on.
        if log.owe_commit {
            commit_frame(&mut log.pending, log.seg, log.next_lsn);
            log.next_lsn += 1;
            log.owe_commit = false;
            log.write_pending()?;
        }
        recovered.removed_segments = log.remove_segments(&obsolete);
        Ok((log, recovered))
    }

    /// Remove segments that no longer count, best effort: one left behind (Windows refuses to delete a file that
    /// is open elsewhere) is harmless, since only the highest finished segment counts, and the next open retries.
    fn remove_segments(&mut self, segs: &[u64]) -> usize {
        let mut n = 0;
        for &s in segs {
            if self.fs.remove(&self.dir.join(seg_name(s))).is_ok() {
                n += 1;
            }
        }
        if n > 0 {
            let _ = self.fs.sync_dir(&self.dir);
        }
        n
    }

    /// Buffer a record. It is durable once a [`Log::sync`] returns an Lsn at or above the one returned here.
    pub fn append(&mut self, kind: u16, payload: &[u8]) -> Result<Lsn, StoreError> {
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }
        if kind >= RESERVED_KINDS {
            return Err(StoreError::Refused("the kind is reserved"));
        }
        if payload.len() > self.opts.max_record as usize {
            return Err(StoreError::Refused("the record is over max_record"));
        }
        if self.pending.is_empty() && self.owe_commit {
            commit_frame(&mut self.pending, self.seg, self.next_lsn);
            self.next_lsn += 1;
            self.owe_commit = false;
        }
        let lsn = self.next_lsn;
        self.index.insert(lsn, self.written + self.pending.len() as u64);
        frame(&mut self.pending, self.seg, lsn, kind, payload);
        self.next_lsn += 1;
        Ok(Lsn(lsn))
    }

    /// Write the buffered records and fsync. Returns the Lsn through which the log is durable.
    pub fn sync(&mut self) -> Result<Lsn, StoreError> {
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }
        if self.pending.is_empty() {
            return Ok(self.durable);
        }
        self.write_pending()?;
        self.owe_commit = true;
        Ok(self.durable)
    }

    fn write_pending(&mut self) -> Result<(), StoreError> {
        if let Err(e) = self.fs.append(&mut self.file, &self.pending) {
            // Part of the batch may be in the file. Cut it back to the synced length; if that works the file is as
            // it was and the batch can be retried. If not, the file's state is unknown.
            if self.fs.truncate(&mut self.file, self.written).is_err() {
                self.poisoned = true;
            }
            return Err(io_err(e));
        }
        if let Err(e) = self.fs.sync(&mut self.file) {
            // After a failed fsync the page cache may have dropped the dirty pages: nothing about the file is known.
            self.poisoned = true;
            return Err(io_err(e));
        }
        self.written += self.pending.len() as u64;
        self.pending.clear();
        self.durable = Lsn(self.next_lsn - 1);
        Ok(())
    }

    /// Replace the log's contents with `live`, atomically. Buffered records are synced first, so every Lsn handed
    /// out before the call is durable or the call fails; after it, the log holds `live` (with new Lsns, returned in
    /// order) and the older segments are gone. On an error before the new segment's commit marker is written, the new
    /// file is removed and the log goes on in the old segment (after [`StoreError::Full`], a retry is safe). On an
    /// error after it, the new segment may count, so the log is poisoned: reopen it.
    pub fn checkpoint(&mut self, live: &[(u16, &[u8])]) -> Result<Vec<Lsn>, StoreError> {
        for (kind, payload) in live {
            if *kind >= RESERVED_KINDS {
                return Err(StoreError::Refused("the kind is reserved"));
            }
            if payload.len() > self.opts.max_record as usize {
                return Err(StoreError::Refused("the record is over max_record"));
            }
        }
        self.sync()?;
        let seg = Seg { n: self.next_segment, salt: self.fs.salt() };
        self.next_segment += 1;
        let base = self.next_lsn;
        // A half-written segment never counts (no commit after its end) and the next open removes it.
        let (file, len, next_lsn, offsets) = match write_segment(&mut self.fs, &self.dir, seg, base, live) {
            Ok(r) => r,
            Err(SegmentError::Before(e)) => return Err(e),
            Err(SegmentError::After(e)) => {
                self.poisoned = true;
                return Err(e);
            }
        };
        self.index = offsets.iter().enumerate().map(|(i, o)| (base + i as u64, *o)).collect();
        let old = self.seg.n;
        self.seg = seg;
        // The old segment's handle closes here, before its delete (Windows refuses to delete an open file).
        self.file = file;
        self.written = len;
        self.next_lsn = next_lsn;
        self.owe_commit = false;
        self.durable = Lsn(next_lsn - 1);
        self.remove_segments(&[old]);
        Ok((0..live.len() as u64).map(|i| Lsn(base + i)).collect())
    }

    /// Read records of the current segment back by Lsn, in the order asked. An Lsn not in it (an earlier
    /// segment's, a marker's, or one not yet written) is skipped, so the caller compares what it got.
    pub fn read(&mut self, lsns: &[Lsn]) -> Result<Vec<Record>, StoreError> {
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }
        let path = self.dir.join(seg_name(self.seg.n));
        let mut out = Vec::with_capacity(lsns.len());
        for l in lsns {
            let Some(&at) = self.index.get(&l.0) else { continue };
            if at >= self.written {
                continue;
            }
            let head = self.fs.read_at(&path, at, FRAME).map_err(io_err)?;
            let len = head.get(..4).and_then(|b| b.try_into().ok()).map_or(0, u32::from_le_bytes) as usize;
            let bytes = self.fs.read_at(&path, at, FRAME + len).map_err(io_err)?;
            match parse_frame(&bytes, 0, self.seg, l.0, self.opts.max_record) {
                Some((kind, payload, _)) if kind < RESERVED_KINDS => {
                    out.push(Record { lsn: *l, kind, payload: payload.to_vec() })
                }
                _ => {
                    let why = "a record read back fails its check";
                    return Err(StoreError::Corrupt { segment: self.seg.n, offset: at, why });
                }
            }
        }
        Ok(out)
    }

    /// A checkpoint whose live set is these records of the current segment, read back from disk: compaction
    /// without holding the bodies in memory. Returns their new Lsns, in order. Buffered records are synced first, so
    /// every Lsn handed out so far can be kept.
    pub fn compact(&mut self, keep: &[Lsn]) -> Result<Vec<Lsn>, StoreError> {
        self.sync()?;
        let recs = self.read(keep)?;
        if recs.len() != keep.len() {
            return Err(StoreError::Refused("compact: an Lsn that isn't a record of the current segment"));
        }
        let live: Vec<(u16, &[u8])> = recs.iter().map(|r| (r.kind, r.payload.as_slice())).collect();
        self.checkpoint(&live)
    }

    /// The Lsn through which the log is durable.
    pub fn durable(&self) -> Lsn {
        self.durable
    }

    /// Bytes on disk in the current segment, and bytes buffered.
    pub fn bytes(&self) -> (u64, u64) {
        (self.written, self.pending.len() as u64)
    }

    pub fn poisoned(&self) -> bool {
        self.poisoned
    }

    /// The file system, for tests.
    pub fn fs(&self) -> &F {
        &self.fs
    }

    /// The file system, for tests that make it fail.
    pub fn fs_mut(&mut self) -> &mut F {
        &mut self.fs
    }

    /// Close the log and hand back its file system (buffered records are dropped, as a crash would).
    pub fn into_fs(self) -> F {
        self.fs
    }
}

/// Write a whole segment (header, live records, end marker), sync it and its folder entry, then its commit marker,
/// synced: the segment counts from that second sync. Returns the file, its length, the next Lsn and each live
/// record's offset.
fn write_segment<F: Fs>(
    fs: &mut F,
    dir: &Path,
    seg: Seg,
    base: u64,
    live: &[(u16, &[u8])],
) -> Result<(F::File, u64, u64, Vec<u64>), SegmentError> {
    let mut bytes = header(seg, base);
    let mut lsn = base;
    let mut offsets = Vec::with_capacity(live.len());
    for (kind, payload) in live {
        offsets.push(bytes.len() as u64);
        frame(&mut bytes, seg, lsn, *kind, payload);
        lsn += 1;
    }
    let mut end = lsn.to_le_bytes().to_vec();
    end.extend_from_slice(&(live.len() as u64).to_le_bytes());
    frame(&mut bytes, seg, lsn, KIND_END, &end);
    lsn += 1;
    let path = dir.join(seg_name(seg.n));
    let mut file = fs.create_new(&path).map_err(|e| SegmentError::Before(io_err(e)))?;
    let body = fs.append(&mut file, &bytes).and_then(|()| fs.sync(&mut file)).and_then(|()| fs.sync_dir(dir));
    if let Err(e) = body {
        // No commit was written, so the segment never counts. Remove it now, so retries on a full disk don't each
        // leave a file holding the space they need (best effort: the next open removes a leftover too).
        drop(file);
        if fs.remove(&path).is_ok() {
            let _ = fs.sync_dir(dir);
        }
        return Err(SegmentError::Before(io_err(e)));
    }
    let mut commit = Vec::new();
    commit_frame(&mut commit, seg, lsn);
    // From here the commit may reach the disk whatever the call returns, and then the new segment counts: on an
    // error only a reopen can tell which segment does.
    fs.append(&mut file, &commit).map_err(|e| SegmentError::After(io_err(e)))?;
    fs.sync(&mut file).map_err(|e| SegmentError::After(io_err(e)))?;
    Ok((file, (bytes.len() + commit.len()) as u64, lsn + 1, offsets))
}

/// Why [`write_segment`] failed: before its commit marker was written (the segment never counts and is removed), or
/// after (it may count).
enum SegmentError {
    Before(StoreError),
    After(StoreError),
}

impl SegmentError {
    fn error(self) -> StoreError {
        match self {
            SegmentError::Before(e) | SegmentError::After(e) => e,
        }
    }
}

/// A simulated file system for crash tests: written bytes are volatile until synced, a created or removed name is
/// volatile until the folder is synced (or, with [`SimFs::ntfs`], the file itself), and the machine can crash after
/// any operation. A crash keeps each file's durable bytes plus a chosen prefix of its volatile ones.
#[cfg(any(test, feature = "test-util"))]
#[derive(Clone, Debug, Default)]
pub struct SimFs {
    files: BTreeMap<PathBuf, SimFile>,
    /// Durable names, as the folder would show them after a crash.
    durable_names: BTreeMap<PathBuf, bool>,
    /// A file's own sync also makes its name durable (NTFS journals the creation; POSIX needs the folder synced).
    pub ntfs: bool,
    /// Operations so far.
    pub ops: usize,
    /// Fail every operation from this count on, as a crash would.
    pub crash_at: Option<usize>,
    /// Fail an append with "disk full" once the total bytes would pass this.
    pub capacity: Option<u64>,
    /// Fail the next fsync, once.
    pub fail_next_sync: bool,
    /// Refuse every remove, as Windows does for a file open elsewhere.
    pub refuse_remove: bool,
    /// Salts handed out, so each is distinct and the runs are repeatable.
    salts: u64,
}

#[cfg(any(test, feature = "test-util"))]
#[derive(Clone, Debug, Default)]
struct SimFile {
    durable: Vec<u8>,
    volatile: Vec<u8>,
    removed: bool,
}

/// A handle to a [`SimFs`] file: its path.
#[cfg(any(test, feature = "test-util"))]
#[derive(Clone, Debug)]
pub struct SimHandle(PathBuf);

#[cfg(any(test, feature = "test-util"))]
impl SimFs {
    pub fn new() -> SimFs {
        SimFs::default()
    }

    fn step(&mut self) -> io::Result<()> {
        if self.crash_at.is_some_and(|c| self.ops >= c) {
            return Err(io::Error::other("crashed"));
        }
        self.ops += 1;
        Ok(())
    }

    fn live(&self, path: &Path) -> io::Result<&SimFile> {
        self.files.get(path).filter(|f| !f.removed).ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    fn total(&self) -> u64 {
        self.files.values().filter(|f| !f.removed).map(|f| (f.durable.len() + f.volatile.len()) as u64).sum()
    }

    /// The files with volatile bytes, and how many each has.
    pub fn volatile(&self) -> Vec<(PathBuf, usize)> {
        self.files.iter().filter(|(_, f)| !f.volatile.is_empty()).map(|(p, f)| (p.clone(), f.volatile.len())).collect()
    }

    /// The state after a crash: each file keeps its durable bytes and `keep(path, volatile)` bytes of its volatile
    /// ones (garbled at byte `garble` of that kept tail, if given); names keep their durable state.
    pub fn crash(&self, keep: impl Fn(&Path, usize) -> (usize, Option<usize>)) -> SimFs {
        let mut files = BTreeMap::new();
        for (p, f) in &self.files {
            let exists = *self.durable_names.get(p).unwrap_or(&false);
            if !exists {
                continue;
            }
            let (n, garble) = keep(p, f.volatile.len());
            let mut durable = f.durable.clone();
            let start = durable.len();
            durable.extend_from_slice(&f.volatile[..n.min(f.volatile.len())]);
            if let Some(g) = garble
                && start + g < durable.len()
            {
                durable[start + g] ^= 0x5a;
            }
            files.insert(p.clone(), SimFile { durable, volatile: Vec::new(), removed: false });
        }
        let durable_names = files.keys().map(|p| (p.clone(), true)).collect();
        SimFs { files, durable_names, ntfs: self.ntfs, salts: self.salts, ..SimFs::default() }
    }

    /// A file's whole content (durable and volatile), for tests that corrupt it.
    pub fn bytes_mut(&mut self, path: &Path) -> Option<&mut Vec<u8>> {
        self.files.get_mut(path).filter(|f| !f.removed && f.volatile.is_empty()).map(|f| &mut f.durable)
    }

    pub fn names(&self) -> Vec<PathBuf> {
        self.files.iter().filter(|(_, f)| !f.removed).map(|(p, _)| p.clone()).collect()
    }
}

#[cfg(any(test, feature = "test-util"))]
impl Fs for SimFs {
    type File = SimHandle;

    fn create_dir_all(&mut self, _dir: &Path) -> io::Result<()> {
        self.step()
    }

    fn list(&mut self, dir: &Path) -> io::Result<Vec<String>> {
        self.step()?;
        Ok(self
            .files
            .iter()
            .filter(|(p, f)| !f.removed && p.parent() == Some(dir))
            .filter_map(|(p, _)| p.file_name().and_then(|n| n.to_str()).map(str::to_owned))
            .collect())
    }

    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        self.step()?;
        let f = self.live(path)?;
        let mut b = f.durable.clone();
        b.extend_from_slice(&f.volatile);
        Ok(b)
    }

    fn read_at(&mut self, path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let all = self.read(path)?;
        let from = (offset as usize).min(all.len());
        Ok(all[from..(from + len).min(all.len())].to_vec())
    }

    fn create_new(&mut self, path: &Path) -> io::Result<SimHandle> {
        self.step()?;
        if self.live(path).is_ok() {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        self.files.insert(path.to_owned(), SimFile::default());
        self.durable_names.entry(path.to_owned()).or_insert(false);
        Ok(SimHandle(path.to_owned()))
    }

    fn open_append(&mut self, path: &Path) -> io::Result<SimHandle> {
        self.step()?;
        self.live(path)?;
        Ok(SimHandle(path.to_owned()))
    }

    fn append(&mut self, file: &mut SimHandle, bytes: &[u8]) -> io::Result<()> {
        self.step()?;
        if let Some(cap) = self.capacity
            && self.total() + bytes.len() as u64 > cap
        {
            // A partial write, then the error, as a real full disk may do.
            let room = cap.saturating_sub(self.total()) as usize;
            if let Some(f) = self.files.get_mut(&file.0) {
                f.volatile.extend_from_slice(&bytes[..room.min(bytes.len())]);
            }
            return Err(io::Error::from(io::ErrorKind::StorageFull));
        }
        let f = self.files.get_mut(&file.0).filter(|f| !f.removed).ok_or(io::ErrorKind::NotFound)?;
        f.volatile.extend_from_slice(bytes);
        Ok(())
    }

    fn sync(&mut self, file: &mut SimHandle) -> io::Result<()> {
        self.step()?;
        if std::mem::take(&mut self.fail_next_sync) {
            return Err(io::Error::other("fsync failed"));
        }
        let ntfs = self.ntfs;
        let f = self.files.get_mut(&file.0).filter(|f| !f.removed).ok_or(io::ErrorKind::NotFound)?;
        let v = std::mem::take(&mut f.volatile);
        f.durable.extend_from_slice(&v);
        if ntfs {
            self.durable_names.insert(file.0.clone(), true);
        }
        Ok(())
    }

    fn truncate(&mut self, file: &mut SimHandle, len: u64) -> io::Result<()> {
        self.step()?;
        let f = self.files.get_mut(&file.0).filter(|f| !f.removed).ok_or(io::ErrorKind::NotFound)?;
        let mut all = std::mem::take(&mut f.durable);
        all.extend_from_slice(&std::mem::take(&mut f.volatile));
        all.truncate(len as usize);
        f.durable = all;
        Ok(())
    }

    fn remove(&mut self, path: &Path) -> io::Result<()> {
        self.step()?;
        if self.refuse_remove {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        let f = self.files.get_mut(path).filter(|f| !f.removed).ok_or(io::ErrorKind::NotFound)?;
        f.removed = true;
        Ok(())
    }

    fn sync_dir(&mut self, dir: &Path) -> io::Result<()> {
        self.step()?;
        let removed: Vec<PathBuf> =
            self.files.iter().filter(|(p, f)| f.removed && p.parent() == Some(dir)).map(|(p, _)| p.clone()).collect();
        for p in removed {
            self.files.remove(&p);
            self.durable_names.remove(&p);
        }
        for (p, _) in self.files.iter().filter(|(p, _)| p.parent() == Some(dir)) {
            self.durable_names.insert(p.clone(), true);
        }
        Ok(())
    }
    fn salt(&mut self) -> [u8; 16] {
        self.salts += 1;
        let mut s = [0x11u8; 16];
        s[..8].copy_from_slice(&self.salts.to_le_bytes());
        s
    }
}

/// One numbered request from a node, in the order it was emitted. Numbers count up from 0 without gaps, per log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Job {
    Append {
        n: u64,
        kind: u16,
        bytes: Vec<u8>,
    },
    /// Replace the log with this live set (see [`Log::checkpoint`]).
    Checkpoint {
        n: u64,
        live: Vec<(u16, Vec<u8>)>,
    },
    /// Replace the log with these of its records (see [`Log::compact`]). One that doesn't fit on the disk is
    /// skipped: its [`Done`] then gives the records' Lsns where they are.
    Compact {
        n: u64,
        keep: Vec<Keep>,
    },
    /// Read these records back ([`Report::Loaded`]). An Lsn that isn't a written record of the current segment is
    /// left out of the answer.
    Load {
        n: u64,
        lsns: Vec<Lsn>,
    },
}

/// A record to keep in a [`Job::Compact`]: by its Lsn, or by the number of the append job that wrote it (when the
/// caller hasn't learnt its Lsn yet: the job may be in the same batch).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    Lsn(Lsn),
    Job(u64),
}

impl Job {
    pub fn n(&self) -> u64 {
        match self {
            Job::Append { n, .. } | Job::Checkpoint { n, .. } | Job::Compact { n, .. } | Job::Load { n, .. } => *n,
        }
    }
}

/// A job that is durable, with the Lsns it gave: an append's one, a checkpoint's or compaction's live set in order,
/// none for a load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Done {
    pub n: u64,
    pub lsns: Vec<Lsn>,
}

/// What one [`Runner::run`] did.
#[derive(Debug, Default)]
pub struct Ran {
    /// The highest job number now durable, if this run made any durable.
    pub durable: Option<u64>,
    /// The jobs that became durable, in order (including ones taken by earlier runs that failed at their sync).
    pub done: Vec<Done>,
    /// Loads answered, in order.
    pub loaded: Vec<(u64, Vec<Record>)>,
    /// What stopped the run, if anything. After [`StoreError::Full`] (and the log not poisoned) the jobs taken stay
    /// buffered and a later run retries them; jobs not taken (from [`Runner::next`] on) must be submitted again.
    pub error: Option<StoreError>,
}

/// Runs jobs against a log, in order, one fsync per run. It remembers the jobs taken but not yet durable, and each
/// append's Lsn since the last checkpoint (for [`Keep::Job`]).
#[derive(Debug, Default)]
pub struct Runner {
    /// The number the next job must carry.
    pub next: u64,
    /// Taken, not yet durable.
    undone: Vec<Done>,
    /// Append job -> its Lsn, in the current segment.
    appended: BTreeMap<u64, Lsn>,
}

impl Runner {
    pub fn new(next: u64) -> Runner {
        Runner { next, ..Runner::default() }
    }

    /// Run jobs in order, then sync once.
    pub fn run<F: Fs>(&mut self, log: &mut Log<F>, jobs: &[Job]) -> Ran {
        let mut ran = Ran::default();
        for j in jobs {
            if j.n() != self.next {
                ran.error = Some(StoreError::Refused("a job out of order"));
                return ran;
            }
            let r = match j {
                Job::Append { n, kind, bytes } => log.append(*kind, bytes).map(|l| {
                    self.appended.insert(*n, l);
                    self.undone.push(Done { n: *n, lsns: vec![l] });
                }),
                Job::Checkpoint { n, live } => {
                    let live: Vec<(u16, &[u8])> = live.iter().map(|(k, b)| (*k, b.as_slice())).collect();
                    log.checkpoint(&live).map(|lsns| self.rebased(*n, lsns))
                }
                Job::Compact { n, keep } => {
                    let lsns: Option<Vec<Lsn>> = keep
                        .iter()
                        .map(|k| match k {
                            Keep::Lsn(l) => Some(*l),
                            Keep::Job(j) => self.appended.get(j).copied(),
                        })
                        .collect();
                    match lsns {
                        // Sync first: a full disk there leaves the batch's appends undurable, an ordinary error.
                        Some(lsns) => log.sync().and_then(|_| match log.compact(&lsns) {
                            Ok(new) => {
                                self.rebased(*n, new);
                                Ok(())
                            }
                            // Compaction is an optimization: one that doesn't fit is skipped, reported with the
                            // records where they are, so it never blocks the jobs behind it.
                            Err(StoreError::Full) if !log.poisoned() => {
                                self.undone.push(Done { n: *n, lsns });
                                Ok(())
                            }
                            Err(e) => Err(e),
                        }),
                        None => Err(StoreError::Refused("compact: an append job this segment doesn't hold")),
                    }
                }
                Job::Load { n, lsns } => log.read(lsns).map(|recs| ran.loaded.push((*n, recs))),
            };
            if let Err(e) = r {
                ran.error = Some(e);
                return ran;
            }
            if let Job::Load { n, .. } = j {
                // A load makes nothing durable, but its number is passed in order like any other.
                self.undone.push(Done { n: *n, lsns: Vec::new() });
            }
            self.next += 1;
        }
        match log.sync() {
            Ok(_) => {
                ran.done = std::mem::take(&mut self.undone);
                ran.durable = ran.done.last().map(|d| d.n);
            }
            Err(e) => ran.error = Some(e),
        }
        ran
    }

    /// A checkpoint or compaction made everything before it durable and started a new segment.
    fn rebased(&mut self, n: u64, lsns: Vec<Lsn>) {
        self.appended.clear();
        self.undone.push(Done { n, lsns });
    }
}

/// Run jobs inline and sync once: [`Runner::run`] for a caller that keeps only the next job number. Returns the
/// highest job number now durable.
pub fn persist<F: Fs>(log: &mut Log<F>, next: &mut u64, jobs: &[Job]) -> Result<Option<u64>, StoreError> {
    let mut r = Runner::new(*next);
    let ran = r.run(log, jobs);
    *next = r.next;
    match ran.error {
        Some(e) => Err(e),
        None => Ok(ran.durable.or(next.checked_sub(1))),
    }
}

/// What the [`Writer`] thread reports.
#[derive(Debug)]
pub enum Report {
    /// Every job through `through` is durable; `done` lists those not reported before, with their Lsns.
    Durable { through: u64, done: Vec<Done> },
    /// A load's records.
    Loaded { n: u64, records: Vec<Record> },
    /// The disk is full. Jobs from `from` on aren't durable yet; the writer retries as more arrive and every `retry`.
    Full { from: u64 },
    /// The log failed and the writer stopped: reopen it (recovery), then start a new writer.
    Failed(StoreError),
}

/// The log on its own thread: jobs go in with [`Writer::submit`], reports come out of [`Writer::poll`]. Everything
/// queued when the thread wakes is committed with one fsync.
pub struct Writer {
    tx: Option<std::sync::mpsc::Sender<Vec<Job>>>,
    rx: std::sync::mpsc::Receiver<Report>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Writer {
    /// Take over `log`; the first job submitted must be number `next`.
    pub fn spawn<F>(mut log: Log<F>, next: u64, retry: std::time::Duration) -> Writer
    where
        F: Fs + Send + 'static,
        F::File: Send,
    {
        let (tx, jobs) = std::sync::mpsc::channel::<Vec<Job>>();
        let (reports, rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pe-agree-store".into())
            .spawn(move || {
                let mut runner = Runner::new(next);
                let mut waiting: Vec<Job> = Vec::new();
                // The first job not yet reported durable.
                let mut undone = next;
                loop {
                    let first = if waiting.is_empty() && undone == runner.next {
                        jobs.recv().map_err(|_| ())
                    } else {
                        match jobs.recv_timeout(retry) {
                            Ok(b) => Ok(b),
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(Vec::new()),
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(()),
                        }
                    };
                    let Ok(first) = first else { return };
                    waiting.extend(first);
                    while let Ok(more) = jobs.try_recv() {
                        waiting.extend(more);
                    }
                    let from = runner.next;
                    let ran = runner.run(&mut log, &waiting);
                    waiting.drain(..(runner.next - from) as usize);
                    for (n, records) in ran.loaded {
                        let _ = reports.send(Report::Loaded { n, records });
                    }
                    if let Some(through) = ran.durable {
                        undone = through + 1;
                        let _ = reports.send(Report::Durable { through, done: ran.done });
                    }
                    match ran.error {
                        None => {}
                        Some(StoreError::Full) if !log.poisoned() => {
                            let _ = reports.send(Report::Full { from: undone });
                        }
                        Some(e) => {
                            let _ = reports.send(Report::Failed(e));
                            return;
                        }
                    }
                }
            })
            .ok();
        Writer { tx: Some(tx), rx, thread }
    }

    /// Queue jobs, in order.
    pub fn submit(&self, jobs: Vec<Job>) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(jobs);
        }
    }

    /// The reports since the last poll, oldest first. Never blocks.
    pub fn poll(&self) -> Vec<Report> {
        self.rx.try_iter().collect()
    }

    /// Wait for the next report, for tests and shutdown.
    pub fn wait(&self, timeout: std::time::Duration) -> Option<Report> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// Whether the thread is running (it stops after [`Report::Failed`]).
    pub fn running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
}

impl Drop for Writer {
    /// Finish the queued jobs, then stop.
    fn drop(&mut self) {
        self.tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
