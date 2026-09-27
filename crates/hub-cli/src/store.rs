//! On-disk `.hub/` storage (Issue #16, format v0 [decision-proposal]).
//!
//! Layout:
//! - `.hub/replica`      `barocss-hub/replica/v0\n<32 hex digits>\n`, written
//!   once at init via write-temp + fsync + rename (+ fsync of `.hub/`).
//! - `.hub/ops.log`      append-only op log.
//! - `.hub/checkpoints.log` append-only local checkpoint list (D-004).
//!
//! Both logs are UTF-8 text: a header line (`barocss-hub/oplog/v0` or
//! `barocss-hub/checkpoints/v0`), then one record per line:
//! `<body>\t<crc>\n`, where `crc` = first 8 hex digits of SHA-256(body).
//! `body` never contains a tab or newline (strings are percent-escaped).
//!
//! Op body: `O <id> <deps> <payload>`; id = `<counter>.<replica 32 hex>`;
//! deps = `-` or comma-joined ids; payload = `C d|f <parent> <str>`,
//! `M <node> <parent> <str>` or `D <node>`.
//! Checkpoint body: `K <author 32 hex> <frontier> <str message> <str note|->`.
//! A string token is `s` followed by the escaped bytes (`s` alone = "").
//!
//! Appends write one whole line and `fsync` before returning. On open, a
//! final record with no trailing newline or a bad checksum is a torn write:
//! it is truncated away (file `set_len` + fsync) and reported. A bad record
//! that is not the last line is corruption and opening fails. A repeated op
//! id with a different op is corruption; an exact repeat is skipped.
//!
//! `.hub/lock` is an advisory `flock` file: mutating commands hold it
//! exclusively from replay to append, read-only commands hold it shared.

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use hub_core::checkpoint::Checkpoint;
use hub_core::tree::{BlobId, Kind, NodeId, Op, Payload, ReplicaId};
use hub_core::version::Store;

pub const OPLOG_HEADER: &str = "barocss-hub/oplog/v0";
pub const CKPT_HEADER: &str = "barocss-hub/checkpoints/v0";
pub const REPLICA_HEADER: &str = "barocss-hub/replica/v0";

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    Corrupt {
        file: PathBuf,
        line: usize,
        why: String,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "i/o error: {e}"),
            StoreError::Corrupt { file, line, why } => {
                write!(f, "corrupt {} line {line}: {why}", file.display())
            }
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        StoreError::Io(e)
    }
}

fn crc(body: &str) -> String {
    let h = Sha256::digest(body.as_bytes());
    h[..4].iter().map(|b| format!("{b:02x}")).collect()
}

pub fn esc(s: &str) -> String {
    let mut out = String::from("s");
    for c in s.chars() {
        if c == '%' || (c as u32) <= 0x20 || c as u32 == 0x7f {
            let _ = write!(out, "%{:02x}", c as u32);
        } else {
            out.push(c);
        }
    }
    out
}

pub fn unesc(t: &str) -> Result<String, String> {
    let t = t
        .strip_prefix('s')
        .ok_or("string token must start with 's'")?;
    let b = t.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = t.get(i + 1..i + 3).ok_or("bad escape")?;
            out.push(u8::from_str_radix(h, 16).map_err(|_| "bad escape")?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "invalid utf-8".to_string())
}

pub fn fmt_id(n: NodeId) -> String {
    format!("{}.{:032x}", n.counter, n.replica.0)
}

pub fn parse_id(t: &str) -> Result<NodeId, String> {
    let (c, r) = t.split_once('.').ok_or("bad id")?;
    if r.len() != 32 {
        return Err("bad replica".into());
    }
    Ok(NodeId::new(
        c.parse().map_err(|_| "bad counter")?,
        u128::from_str_radix(r, 16).map_err(|_| "bad replica")?,
    ))
}

fn fmt_ids(ids: impl IntoIterator<Item = NodeId>) -> String {
    let v: Vec<String> = ids.into_iter().map(fmt_id).collect();
    if v.is_empty() {
        "-".into()
    } else {
        v.join(",")
    }
}

fn parse_ids(t: &str) -> Result<Vec<NodeId>, String> {
    if t == "-" {
        return Ok(Vec::new());
    }
    t.split(',').map(parse_id).collect()
}

fn parse_blob(t: &str) -> Result<BlobId, String> {
    if t.len() != 64 || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("bad blob".into());
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&t[2 * i..2 * i + 2], 16).map_err(|_| "bad blob")?;
    }
    Ok(BlobId(out))
}

pub fn encode_op(op: &Op) -> String {
    let p = match &op.payload {
        Payload::Create { kind, parent, name } => {
            let k = match kind {
                Kind::File => "f",
                _ => "d",
            };
            format!("C {k} {} {}", fmt_id(*parent), esc(name))
        }
        Payload::Move {
            node,
            new_parent,
            new_name,
        } => format!(
            "M {} {} {}",
            fmt_id(*node),
            fmt_id(*new_parent),
            esc(new_name)
        ),
        Payload::Delete { node } => format!("D {}", fmt_id(*node)),
        Payload::SetContent { node, blob } => format!("S {} {}", fmt_id(*node), blob.hex()),
    };
    format!(
        "O {} {} {p}",
        fmt_id(op.id),
        fmt_ids(op.deps.iter().copied())
    )
}

pub fn decode_op(body: &str) -> Result<Op, String> {
    let t: Vec<&str> = body.split(' ').collect();
    let (id, deps, rest) = match t.as_slice() {
        ["O", id, deps, rest @ ..] => (parse_id(id)?, parse_ids(deps)?, rest),
        _ => return Err("not an op record".into()),
    };
    let payload = match rest {
        ["C", k, parent, name] => Payload::Create {
            kind: match *k {
                "f" => Kind::File,
                "d" => Kind::Directory,
                _ => return Err("bad kind".into()),
            },
            parent: parse_id(parent)?,
            name: unesc(name)?,
        },
        ["M", node, parent, name] => Payload::Move {
            node: parse_id(node)?,
            new_parent: parse_id(parent)?,
            new_name: unesc(name)?,
        },
        ["D", node] => Payload::Delete {
            node: parse_id(node)?,
        },
        ["S", node, blob] => Payload::SetContent {
            node: parse_id(node)?,
            blob: parse_blob(blob)?,
        },
        _ => return Err("bad payload".into()),
    };
    Ok(Op {
        id,
        deps: deps.into_iter().collect(),
        payload,
    })
}

pub fn encode_ckpt(c: &Checkpoint) -> String {
    format!(
        "K {:032x} {} {} {}",
        c.author.0,
        fmt_ids(c.frontier().iter().copied()),
        esc(&c.message),
        c.note.as_deref().map(esc).unwrap_or_else(|| "-".into())
    )
}

pub fn decode_ckpt(body: &str, store: &Store) -> Result<Checkpoint, String> {
    let t: Vec<&str> = body.split(' ').collect();
    let ["K", author, frontier, msg, note] = t.as_slice() else {
        return Err("not a checkpoint record".into());
    };
    let author = ReplicaId(u128::from_str_radix(author, 16).map_err(|_| "bad author")?);
    let note = if *note == "-" {
        None
    } else {
        Some(unesc(note)?)
    };
    Checkpoint::from_frontier(store, parse_ids(frontier)?, unesc(msg)?, author, note)
        .map_err(|e| format!("checkpoint frontier not in op log: {e:?}"))
}

/// Result of reading a log: record bodies plus whether a torn tail was cut.
pub struct LogRead {
    pub records: Vec<String>,
    pub truncated_tail: bool,
}

/// Read a log, validating header and checksums; truncate a torn final record.
pub fn read_log(path: &Path, header: &str) -> Result<LogRead, StoreError> {
    let bytes = fs::read(path)?;
    let corrupt = |line: usize, why: &str| StoreError::Corrupt {
        file: path.to_path_buf(),
        line,
        why: why.to_string(),
    };
    let head = format!("{header}\n");
    if !bytes.starts_with(head.as_bytes()) {
        return Err(corrupt(1, "missing or wrong header"));
    }
    let mut records = Vec::new();
    let mut pos = head.len();
    let mut lineno = 1;
    let mut good_end = pos;
    let mut torn = false;
    while pos < bytes.len() {
        lineno += 1;
        let nl = bytes[pos..].iter().position(|&b| b == b'\n');
        let (line, next, complete) = match nl {
            Some(i) => (&bytes[pos..pos + i], pos + i + 1, true),
            None => (&bytes[pos..], bytes.len(), false),
        };
        let is_last = next >= bytes.len();
        let parsed = std::str::from_utf8(line)
            .ok()
            .and_then(|s| s.rsplit_once('\t'))
            .filter(|(body, c)| *c == crc(body))
            .map(|(body, _)| body.to_string());
        match parsed {
            Some(body) if complete => {
                records.push(body);
                good_end = next;
            }
            _ if is_last => {
                torn = true;
                break;
            }
            _ => return Err(corrupt(lineno, "bad checksum or framing")),
        }
        pos = next;
    }
    if torn {
        let f = OpenOptions::new().write(true).open(path)?;
        f.set_len(good_end as u64)?;
        f.sync_all()?;
    }
    Ok(LogRead {
        records,
        truncated_tail: torn,
    })
}

/// One framed record line: `<body>\t<crc>\n`.
pub fn frame(body: &str) -> String {
    debug_assert!(!body.contains('\t') && !body.contains('\n'));
    format!("{body}\t{}\n", crc(body))
}

/// Append one record and fsync.
pub fn append_record(path: &Path, body: &str) -> io::Result<()> {
    let mut f = OpenOptions::new().append(true).open(path)?;
    f.write_all(frame(body).as_bytes())?;
    f.sync_data()
}

/// Crash-safe whole-file write: temp file, fsync, rename, fsync dir.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().unwrap().to_string_lossy()
    ));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    File::open(dir)?.sync_all()
}

/// Lock mode for `.hub/lock` (Issue #16 repair).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockMode {
    /// Readers: excludes writers, admits other readers.
    Shared,
    /// Writers: held across open (replay) -> mint/commit -> append.
    Exclusive,
}

/// An advisory whole-repo lock: `flock(2)` on `.hub/lock`, blocking until
/// granted and released when the file is closed (drop or process exit, so a
/// crashed holder never leaves a stale lock).
///
/// `flock` is declared directly against the platform C library (no new
/// crate); std's `File::lock` needs Rust 1.89. On non-unix targets this is a
/// no-op; concurrent use there is unsupported in v0.
#[derive(Debug)]
pub struct RepoLock {
    _file: File,
}

#[cfg(unix)]
mod sys {
    use std::os::raw::c_int;
    // Same values on Linux, macOS and the BSDs.
    pub const LOCK_SH: c_int = 1;
    pub const LOCK_EX: c_int = 2;
    extern "C" {
        pub fn flock(fd: c_int, operation: c_int) -> c_int;
    }
}

impl RepoLock {
    pub fn acquire(dir: &Path, mode: LockMode) -> io::Result<RepoLock> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("lock"))?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let op = match mode {
                LockMode::Shared => sys::LOCK_SH,
                LockMode::Exclusive => sys::LOCK_EX,
            };
            loop {
                // SAFETY: valid open fd owned by `file`; flock has no memory effects.
                if unsafe { sys::flock(file.as_raw_fd(), op) } == 0 {
                    break;
                }
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }
        #[cfg(not(unix))]
        let _ = mode;
        Ok(RepoLock { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_content_round_trip() {
        let op = Op {
            id: NodeId::new(7, 0xabc),
            deps: [NodeId::new(3, 0xabc)].into_iter().collect(),
            payload: Payload::SetContent {
                node: NodeId::new(3, 0xabc),
                blob: BlobId::of(b"hello"),
            },
        };
        let line = encode_op(&op);
        assert!(line.contains(&format!(
            " S 3.{:032x} {}",
            0xabc,
            BlobId::of(b"hello").hex()
        )));
        assert_eq!(decode_op(&line).unwrap(), op);
        assert!(decode_op(&line.replace(&BlobId::of(b"hello").hex(), "zz")).is_err());
    }
}
