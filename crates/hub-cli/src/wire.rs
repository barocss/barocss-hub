//! Wire protocol v1 codec and transports (Issue #43, Wiki Remote-Sync §5.2).
//!
//! * Preamble `BHUBWIRE`, then frames `len: u32 BE | type: u8 | payload`.
//! * Text payloads reuse the Appendix A token forms (`store::fmt_id` ids,
//!   64-hex blob ids, `s<escaped>` strings, `ops.log` op bodies); `Chunk`
//!   is the only binary payload (bytes + 32-byte SHA-256 trailer).
//! * Limits are checked before allocation: the length prefix is compared
//!   with `max_frame` before the payload is read.
//! * [`Conn`] is one side of a connection: a byte source drained by a
//!   channel (a reader thread for pipes, direct for [`pair`]) and a sink.
//!   Reads have a deadline (step timeout). [`pair`] is the in-memory duplex
//!   transport used by tests; it can be cut after the k-th frame sent by
//!   either side (P3).
//!
//! `RefMoves` (0x0B) belongs to feature `refs`, which #43 never announces:
//! receiving it is `unsupported` (refs are not implemented on develop).

use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hub_core::checkpoint::Checkpoint;
use hub_core::tree::{BlobId, Op, OpId, ReplicaId};
use sha2::{Digest, Sha256};

use crate::store;
use crate::{Code, Error};

pub const MAGIC: &[u8; 8] = b"BHUBWIRE";
pub const PROTO_MIN: u32 = 1;
pub const PROTO_MAX: u32 = 1;
pub const FORMAT: u32 = 1;
pub const FORMAT_MINOR_MAX: u32 = 0;

pub const HELLO: u8 = 0x01;
pub const HAVE: u8 = 0x02;
pub const OPS: u8 = 0x03;
pub const NEED: u8 = 0x04;
pub const NEED_BLOBS: u8 = 0x05;
pub const BLOB_BEGIN: u8 = 0x06;
pub const CHUNK: u8 = 0x07;
pub const BLOB_END: u8 = 0x08;
pub const NO_BLOB: u8 = 0x09;
pub const CHECKPOINTS: u8 = 0x0A;
pub const REF_MOVES: u8 = 0x0B;
pub const DONE: u8 = 0x0C;
pub const COMMITTED: u8 = 0x0D;
pub const ERROR: u8 = 0x0E;
pub const PROGRESS: u8 = 0x0F;
pub const BYE: u8 = 0x10;

/// Floors and hard caps of the announced limits (§5.2.3, R4).
pub const FRAME_FLOOR: u32 = 256 * 1024 + 64;
pub const FRAME_CAP: u32 = 4 << 20;
pub const CHUNK_FLOOR: u32 = 16 * 1024;
pub const CHUNK_CAP: u32 = 1 << 20;
pub const OPS_CAP: u32 = 65_536;
/// Ids per `Need`/`NeedBlobs`/frontier list (§5.2.5).
pub const MAX_LIST: usize = 65_536;
/// Default `max_blob_size` (§5.2.5).
pub const MAX_BLOB_SIZE: u64 = 2 << 30;
/// Longest checkpoint message or note in bytes (§5.2.5, R4).
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

pub const FEATURES: &[&str] = &["blobs-chunked", "checkpoints", "resume"];
pub const F_READ_ONLY: &str = "read-only";

/// Step timeout (no complete frame) and `Hello` timeout (§5.2.5).
pub fn step_timeout() -> Duration {
    test_ms("HUB_TEST_WIRE_TIMEOUT_MS").unwrap_or(Duration::from_secs(60))
}
pub fn hello_timeout() -> Duration {
    test_ms("HUB_TEST_WIRE_TIMEOUT_MS").unwrap_or(Duration::from_secs(15))
}
fn test_ms(k: &str) -> Option<Duration> {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
}

/// What a side accepts (announced in `Hello`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub frame: u32,
    pub chunk: u32,
    pub ops: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            frame: 1 << 20,
            chunk: 256 * 1024,
            ops: 1024,
        }
    }
}

impl Limits {
    /// Floors and caps (§5.2.3): a violation is fatal `protocol`.
    pub fn check(&self) -> Result<(), String> {
        if self.frame < FRAME_FLOOR || self.frame > FRAME_CAP {
            return Err(format!("frame limit {} out of range", self.frame));
        }
        if self.chunk < CHUNK_FLOOR || self.chunk > CHUNK_CAP || self.chunk > self.frame - 33 {
            return Err(format!("chunk limit {} out of range", self.chunk));
        }
        if self.ops < 1 || self.ops > OPS_CAP {
            return Err(format!("ops limit {} out of range", self.ops));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub proto_min: u32,
    pub proto_max: u32,
    pub format: u32,
    pub format_minor_max: u32,
    pub replica: ReplicaId,
    pub features: Vec<String>,
    pub limits: Limits,
}

impl Hello {
    pub fn new(replica: ReplicaId, read_only: bool) -> Hello {
        let mut features: Vec<String> = FEATURES.iter().map(|s| s.to_string()).collect();
        if read_only {
            features.push(F_READ_ONLY.into());
        }
        Hello {
            proto_min: PROTO_MIN,
            proto_max: PROTO_MAX,
            format: FORMAT,
            format_minor_max: FORMAT_MINOR_MAX,
            replica,
            features,
            limits: Limits::default(),
        }
    }
    pub fn has(&self, f: &str) -> bool {
        self.features.iter().any(|x| x == f)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    Hello(Hello),
    Have {
        frontier: Vec<OpId>,
    },
    Ops {
        frontier: Vec<OpId>,
        done: bool,
        ops: Vec<Op>,
    },
    Need(Vec<OpId>),
    NeedBlobs(Vec<(BlobId, u64)>),
    BlobBegin {
        id: BlobId,
        size: u64,
        offset: u64,
    },
    Chunk {
        data: Vec<u8>,
        sha: [u8; 32],
    },
    BlobEnd(BlobId),
    NoBlob(BlobId),
    Checkpoints(Vec<Checkpoint>),
    Done(String),
    Committed(Vec<OpId>),
    Error {
        code: String,
        message: String,
        fatal: bool,
    },
    Progress {
        phase: String,
        done: u64,
        total: Option<u64>,
    },
    Bye,
}

impl Frame {
    pub fn ty(&self) -> u8 {
        match self {
            Frame::Hello(_) => HELLO,
            Frame::Have { .. } => HAVE,
            Frame::Ops { .. } => OPS,
            Frame::Need(_) => NEED,
            Frame::NeedBlobs(_) => NEED_BLOBS,
            Frame::BlobBegin { .. } => BLOB_BEGIN,
            Frame::Chunk { .. } => CHUNK,
            Frame::BlobEnd(_) => BLOB_END,
            Frame::NoBlob(_) => NO_BLOB,
            Frame::Checkpoints(_) => CHECKPOINTS,
            Frame::Done(_) => DONE,
            Frame::Committed(_) => COMMITTED,
            Frame::Error { .. } => ERROR,
            Frame::Progress { .. } => PROGRESS,
            Frame::Bye => BYE,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Frame::Hello(_) => "Hello",
            Frame::Have { .. } => "Have",
            Frame::Ops { .. } => "Ops",
            Frame::Need(_) => "Need",
            Frame::NeedBlobs(_) => "NeedBlobs",
            Frame::BlobBegin { .. } => "BlobBegin",
            Frame::Chunk { .. } => "Chunk",
            Frame::BlobEnd(_) => "BlobEnd",
            Frame::NoBlob(_) => "NoBlob",
            Frame::Checkpoints(_) => "Checkpoints",
            Frame::Done(_) => "Done",
            Frame::Committed(_) => "Committed",
            Frame::Error { .. } => "Error",
            Frame::Progress { .. } => "Progress",
            Frame::Bye => "Bye",
        }
    }
    /// A chunk frame for `data` (trailer computed).
    pub fn chunk(data: Vec<u8>) -> Frame {
        let sha: [u8; 32] = Sha256::digest(&data).into();
        Frame::Chunk { data, sha }
    }
}

fn perr(m: impl Into<String>) -> Error {
    Error::new(Code::Protocol, m.into())
}
fn lerr(m: impl Into<String>) -> Error {
    Error::new(Code::Limit, m.into())
}

pub fn fmt_ids(ids: &[OpId]) -> String {
    if ids.is_empty() {
        "-".into()
    } else {
        ids.iter()
            .map(|i| store::fmt_id(*i))
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn parse_ids(t: &str) -> Result<Vec<OpId>, Error> {
    if t == "-" {
        return Ok(Vec::new());
    }
    let n = t.bytes().filter(|b| *b == b',').count() + 1;
    if n > MAX_LIST {
        return Err(lerr(format!("list of {n} ids exceeds {MAX_LIST}")));
    }
    t.split(',')
        .map(|x| store::parse_id(x).map_err(|e| perr(format!("bad op id: {e}"))))
        .collect()
}

fn num(t: &str) -> Result<u64, Error> {
    if t.is_empty() || t.len() > 20 || !t.bytes().all(|b| b.is_ascii_digit()) {
        return Err(perr(format!("bad number {t:?}")));
    }
    t.parse().map_err(|_| perr(format!("bad number {t:?}")))
}

fn num32(t: &str) -> Result<u32, Error> {
    u32::try_from(num(t)?).map_err(|_| perr("number out of range"))
}

fn blob(t: &str) -> Result<BlobId, Error> {
    store::parse_blob(t).map_err(|e| Error::new(Code::BadBlobId, e))
}

fn flag(t: &str) -> Result<bool, Error> {
    match t {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(perr(format!("bad flag {t:?}"))),
    }
}

/// Encode a frame's type byte and payload.
pub fn encode(f: &Frame) -> Vec<u8> {
    let text = |s: String| s.into_bytes();
    let payload = match f {
        Frame::Hello(h) => text(format!(
            "proto {} {} format {} {} replica {:032x} features {} limits frame={} chunk={} ops={}",
            h.proto_min,
            h.proto_max,
            h.format,
            h.format_minor_max,
            h.replica.0,
            if h.features.is_empty() {
                "-".into()
            } else {
                h.features.join(",")
            },
            h.limits.frame,
            h.limits.chunk,
            h.limits.ops
        )),
        Frame::Have { frontier } => text(format!("{} -", fmt_ids(frontier))),
        Frame::Ops {
            frontier,
            done,
            ops,
        } => {
            let mut s = format!("{} {}", fmt_ids(frontier), *done as u8);
            for o in ops {
                s.push('\n');
                s.push_str(&store::encode_op(o));
            }
            text(s)
        }
        Frame::Need(ids) | Frame::Committed(ids) => text(fmt_ids(ids)),
        Frame::NeedBlobs(v) => text(if v.is_empty() {
            "-".into()
        } else {
            v.iter()
                .map(|(b, o)| {
                    if *o == 0 {
                        b.hex()
                    } else {
                        format!("{}@{o}", b.hex())
                    }
                })
                .collect::<Vec<_>>()
                .join(",")
        }),
        Frame::BlobBegin { id, size, offset } => text(format!("{} {size} {offset}", id.hex())),
        Frame::Chunk { data, sha } => {
            let mut v = data.clone();
            v.extend_from_slice(sha);
            v
        }
        Frame::BlobEnd(id) | Frame::NoBlob(id) => text(id.hex()),
        Frame::Checkpoints(cs) => text(
            cs.iter()
                .map(store::encode_ckpt)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        Frame::Done(p) => text(p.clone()),
        Frame::Error {
            code,
            message,
            fatal,
        } => text(format!("{code} {} {}", store::esc(message), *fatal as u8)),
        Frame::Progress { phase, done, total } => text(format!(
            "{phase} {done} {}",
            total.map(|t| t.to_string()).unwrap_or_else(|| "-".into())
        )),
        Frame::Bye => Vec::new(),
    };
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(f.ty());
    out.extend_from_slice(&payload);
    out
}

/// Decode a frame from its type byte and payload. `ours` are the limits we
/// announced (ops per frame, chunk size).
pub fn decode(ty: u8, p: &[u8], ours: &Limits) -> Result<Frame, Error> {
    if ty == CHUNK {
        if p.len() < 32 {
            return Err(perr("short Chunk"));
        }
        let (d, h) = p.split_at(p.len() - 32);
        if d.len() > ours.chunk as usize {
            return Err(lerr(format!(
                "chunk of {} bytes exceeds {}",
                d.len(),
                ours.chunk
            )));
        }
        let mut sha = [0u8; 32];
        sha.copy_from_slice(h);
        return Ok(Frame::Chunk {
            data: d.to_vec(),
            sha,
        });
    }
    let s = std::str::from_utf8(p).map_err(|_| perr("payload is not UTF-8"))?;
    let words = |n: usize| -> Result<Vec<&str>, Error> {
        let w: Vec<&str> = s.split(' ').collect();
        if w.len() != n {
            return Err(perr(format!("frame 0x{ty:02x}: expected {n} fields")));
        }
        Ok(w)
    };
    Ok(match ty {
        HELLO => {
            let w = words(14)?;
            if w[0] != "proto"
                || w[3] != "format"
                || w[6] != "replica"
                || w[8] != "features"
                || w[10] != "limits"
            {
                return Err(perr("malformed Hello"));
            }
            if w[7].len() != 32 {
                return Err(perr("bad replica id in Hello"));
            }
            let replica =
                u128::from_str_radix(w[7], 16).map_err(|_| perr("bad replica id in Hello"))?;
            let kv = |t: &str, k: &str| -> Result<u32, Error> {
                t.strip_prefix(k)
                    .and_then(|v| v.strip_prefix('='))
                    .ok_or_else(|| perr("malformed Hello limits"))
                    .and_then(num32)
            };
            let features = if w[9] == "-" {
                Vec::new()
            } else {
                w[9].split(',').map(String::from).collect()
            };
            Frame::Hello(Hello {
                proto_min: num32(w[1])?,
                proto_max: num32(w[2])?,
                format: num32(w[4])?,
                format_minor_max: num32(w[5])?,
                replica: ReplicaId(replica),
                features,
                limits: Limits {
                    frame: kv(w[11], "frame")?,
                    chunk: kv(w[12], "chunk")?,
                    ops: kv(w[13], "ops")?,
                },
            })
        }
        HAVE => {
            let w = words(2)?;
            Frame::Have {
                frontier: parse_ids(w[0])?,
            }
        }
        OPS => {
            let mut lines = s.split('\n');
            let head: Vec<&str> = lines.next().unwrap_or("").split(' ').collect();
            let [f, d] = head.as_slice() else {
                return Err(perr("malformed Ops"));
            };
            let bodies: Vec<&str> = lines.collect();
            if bodies.len() > ours.ops as usize {
                return Err(lerr(format!(
                    "{} ops in one frame exceed {}",
                    bodies.len(),
                    ours.ops
                )));
            }
            let ops = bodies
                .iter()
                .map(|b| store::decode_op(b).map_err(|e| perr(format!("bad op record: {e}"))))
                .collect::<Result<Vec<_>, _>>()?;
            Frame::Ops {
                frontier: parse_ids(f)?,
                done: flag(d)?,
                ops,
            }
        }
        NEED => Frame::Need(parse_ids(s)?),
        COMMITTED => Frame::Committed(parse_ids(s)?),
        NEED_BLOBS => {
            if s == "-" {
                Frame::NeedBlobs(Vec::new())
            } else {
                let n = s.bytes().filter(|b| *b == b',').count() + 1;
                if n > MAX_LIST {
                    return Err(lerr(format!("list of {n} blob ids exceeds {MAX_LIST}")));
                }
                let v = s
                    .split(',')
                    .map(|t| match t.split_once('@') {
                        Some((b, o)) => Ok((blob(b)?, num(o)?)),
                        None => Ok((blob(t)?, 0)),
                    })
                    .collect::<Result<Vec<_>, Error>>()?;
                if v.windows(2).any(|w| w[0].0 >= w[1].0) {
                    return Err(perr("NeedBlobs ids not sorted and deduplicated"));
                }
                Frame::NeedBlobs(v)
            }
        }
        BLOB_BEGIN => {
            let w = words(3)?;
            Frame::BlobBegin {
                id: blob(w[0])?,
                size: num(w[1])?,
                offset: num(w[2])?,
            }
        }
        BLOB_END => Frame::BlobEnd(blob(s)?),
        NO_BLOB => Frame::NoBlob(blob(s)?),
        CHECKPOINTS => {
            let mut cs = Vec::new();
            if !s.is_empty() {
                for b in s.split('\n') {
                    let (ids, msg, author, note) =
                        store::split_ckpt(b).map_err(|e| perr(format!("bad checkpoint: {e}")))?;
                    if ids.len() > MAX_LIST {
                        return Err(lerr("checkpoint frontier too long"));
                    }
                    let ids: Vec<OpId> = ids
                        .into_iter()
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    cs.push(Checkpoint::from_canonical(ids, msg, author, note));
                }
            }
            Frame::Checkpoints(cs)
        }
        DONE => match s {
            "ops" | "blobs" | "meta" => Frame::Done(s.into()),
            _ => return Err(perr(format!("bad Done phase {s:?}"))),
        },
        ERROR => {
            let w = words(3)?;
            if w[0].is_empty() || !w[0].bytes().all(|b| b.is_ascii_lowercase() || b == b'-') {
                return Err(perr("bad error code"));
            }
            Frame::Error {
                code: w[0].into(),
                message: store::unesc(w[1]).map_err(|e| perr(format!("bad error message: {e}")))?,
                fatal: flag(w[2])?,
            }
        }
        PROGRESS => {
            let w = words(3)?;
            Frame::Progress {
                phase: w[0].into(),
                done: num(w[1])?,
                total: if w[2] == "-" { None } else { Some(num(w[2])?) },
            }
        }
        BYE => {
            if !p.is_empty() {
                return Err(perr("Bye with a payload"));
            }
            Frame::Bye
        }
        REF_MOVES => {
            return Err(Error::new(
                Code::Unsupported,
                "RefMoves frame without the negotiated feature `refs`",
            ))
        }
        t => return Err(perr(format!("unknown frame type 0x{t:02x}"))),
    })
}

/// Map a wire error code to a CLI [`Code`] (§5.2.6).
pub fn code_of(s: &str) -> Code {
    Code::ALL
        .iter()
        .copied()
        .find(|c| c.as_str() == s)
        .unwrap_or(Code::Error)
}

// ---------------------------------------------------------------- transport

/// Test hook shared by both ends of a [`pair`]: once set, nothing more is
/// delivered in either direction and readers see EOF after the frames
/// already delivered.
#[derive(Clone, Default)]
pub struct Cut {
    flag: Arc<AtomicBool>,
}

impl Cut {
    pub fn is_cut(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
    pub fn cut(&self) {
        self.flag.store(true, Ordering::SeqCst)
    }
}

struct ChanWriter {
    tx: Option<Sender<Vec<u8>>>,
    cut: Cut,
}

impl Write for ChanWriter {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if self.cut.is_cut() {
            self.tx = None;
        }
        match &self.tx {
            Some(tx) if tx.send(b.to_vec()).is_ok() => Ok(b.len()),
            _ => Err(io::Error::new(io::ErrorKind::BrokenPipe, "connection cut")),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// One side of a connection.
pub struct Conn {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
    tx: Box<dyn Write + Send>,
    cut: Cut,
    /// Cut the connection after this many frames sent by this side.
    cut_after: Option<usize>,
    pub frames_out: usize,
    pub frames_in: usize,
    /// Every frame sent (type + payload), when recording (P2).
    pub transcript: Option<Arc<Mutex<Vec<Vec<u8>>>>>,
    /// The limits we announced (frames we accept).
    pub ours: Limits,
    /// The peer's limits (frames we may send), known after `Hello`.
    pub theirs: Limits,
    /// Bytes read so far (tests: prove limits are checked pre-allocation).
    pub bytes_in: Arc<AtomicUsize>,
}

/// In-memory duplex pair (tests). The shared [`Cut`] can end both
/// directions at once.
pub fn pair() -> (Conn, Conn, Cut) {
    let cut = Cut::default();
    let (ta, rb) = mpsc::channel();
    let (tb, ra) = mpsc::channel();
    let mk = |rx, tx| {
        Conn::new(
            rx,
            Box::new(ChanWriter {
                tx: Some(tx),
                cut: cut.clone(),
            }),
            cut.clone(),
        )
    };
    (mk(ra, ta), mk(rb, tb), cut)
}

impl Conn {
    fn new(rx: Receiver<Vec<u8>>, tx: Box<dyn Write + Send>, cut: Cut) -> Conn {
        Conn {
            rx,
            buf: Vec::new(),
            pos: 0,
            eof: false,
            tx,
            cut,
            cut_after: None,
            frames_out: 0,
            frames_in: 0,
            transcript: None,
            ours: Limits::default(),
            theirs: Limits::default(),
            bytes_in: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// A connection over a byte stream pair (process pipes). A thread
    /// drains `r` into an unbounded channel, so a peer's writes never block
    /// on us (both sides send before reading in the meta phase).
    pub fn from_io(r: impl Read + Send + 'static, w: impl Write + Send + 'static) -> Conn {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = r;
            let mut b = vec![0u8; 64 * 1024];
            loop {
                match r.read(&mut b) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(Vec::new());
                        return;
                    }
                    Ok(n) => {
                        if tx.send(b[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        Conn::new(rx, Box::new(io::BufWriter::new(w)), Cut::default())
    }

    /// Cut the connection right after this side has sent `k` frames.
    pub fn cut_after(&mut self, k: usize) {
        self.cut_after = Some(k);
    }

    fn fill(&mut self, deadline: Instant) -> Result<(), Error> {
        if self.pos > 0 && self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        }
        loop {
            if self.eof {
                return Err(Error::new(Code::Io, "connection closed by the remote"));
            }
            let now = Instant::now();
            let left = deadline.saturating_duration_since(now);
            match self.rx.recv_timeout(left.max(Duration::from_millis(1))) {
                Ok(v) if v.is_empty() => self.eof = true,
                Ok(v) => {
                    self.bytes_in.fetch_add(v.len(), Ordering::Relaxed);
                    self.buf.extend_from_slice(&v);
                    return Ok(());
                }
                Err(RecvTimeoutError::Disconnected) => self.eof = true,
                Err(RecvTimeoutError::Timeout) => {
                    if self.cut.is_cut() {
                        self.eof = true;
                        continue;
                    }
                    if Instant::now() >= deadline {
                        return Err(Error::new(
                            Code::Timeout,
                            "no data from the remote within the step timeout",
                        ));
                    }
                }
            }
        }
    }

    fn read_exact(&mut self, n: usize, deadline: Instant) -> Result<Vec<u8>, Error> {
        while self.buf.len() - self.pos < n {
            self.fill(deadline)?;
        }
        let v = self.buf[self.pos..self.pos + n].to_vec();
        self.pos += n;
        Ok(v)
    }

    /// Write the preamble.
    pub fn send_magic(&mut self) -> Result<(), Error> {
        self.tx.write_all(MAGIC)?;
        self.tx.flush()?;
        Ok(())
    }

    /// Read and check the peer's preamble.
    pub fn recv_magic(&mut self, timeout: Duration) -> Result<(), Error> {
        let m = self.read_exact(8, Instant::now() + timeout)?;
        if m != MAGIC {
            return Err(perr(
                "the remote did not answer with the barocss-hub wire preamble \
                 (not barocss-hub, or a login banner on stdout)",
            ));
        }
        Ok(())
    }

    /// Send one frame. Fails with `limit` if it exceeds the peer's frame
    /// limit (callers split lists and batches beforehand).
    pub fn send(&mut self, f: &Frame) -> Result<(), Error> {
        let body = encode(f);
        if body.len() > self.theirs.frame as usize {
            return Err(lerr(format!(
                "{} frame of {} bytes exceeds the remote's limit {}",
                f.name(),
                body.len(),
                self.theirs.frame
            )));
        }
        self.send_raw(&body)
    }

    /// Send pre-encoded frame bytes (type + payload) as one frame.
    pub fn send_raw(&mut self, body: &[u8]) -> Result<(), Error> {
        if self.cut.is_cut() {
            return Err(Error::new(Code::Io, "connection closed"));
        }
        let mut v = Vec::with_capacity(body.len() + 4);
        v.extend_from_slice(&(body.len() as u32).to_be_bytes());
        v.extend_from_slice(body);
        self.tx.write_all(&v)?;
        self.tx.flush()?;
        if let Some(t) = &self.transcript {
            t.lock().unwrap().push(body.to_vec());
        }
        self.frames_out += 1;
        if self.cut_after == Some(self.frames_out) {
            self.cut.cut();
        }
        Ok(())
    }

    /// Read one frame: the length is checked against our frame limit before
    /// the payload is read or allocated.
    pub fn recv(&mut self, timeout: Duration) -> Result<Frame, Error> {
        let deadline = Instant::now() + timeout;
        let l = self.read_exact(4, deadline)?;
        let len = u32::from_be_bytes([l[0], l[1], l[2], l[3]]);
        if len == 0 {
            return Err(perr("zero-length frame"));
        }
        if len > self.ours.frame {
            return Err(lerr(format!(
                "frame of {len} bytes exceeds the limit {}",
                self.ours.frame
            )));
        }
        let b = self.read_exact(len as usize, deadline)?;
        self.frames_in += 1;
        decode(b[0], &b[1..], &self.ours)
    }

    /// Best-effort: send an `Error` frame (the connection may be gone).
    pub fn send_error(&mut self, e: &Error, fatal: bool) {
        let _ = self.send(&Frame::Error {
            code: e.code().as_str().into(),
            message: e.0.clone(),
            fatal,
        });
    }
}

/// Split `items` into groups whose encoded sizes (`size(item)` + 1 each)
/// stay under `max_bytes` and whose counts stay under `max_items`.
pub fn groups<T>(
    items: Vec<T>,
    max_items: usize,
    max_bytes: usize,
    size: impl Fn(&T) -> usize,
) -> Vec<Vec<T>> {
    let mut out: Vec<Vec<T>> = Vec::new();
    let mut cur: Vec<T> = Vec::new();
    let mut bytes = 0;
    for it in items {
        let s = size(&it) + 1;
        if !cur.is_empty() && (cur.len() >= max_items || bytes + s > max_bytes) {
            out.push(std::mem::take(&mut cur));
            bytes = 0;
        }
        bytes += s;
        cur.push(it);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hub_core::tree::{Kind, NodeId, Payload, ROOT};

    fn rt(f: Frame) {
        let e = encode(&f);
        let g = decode(e[0], &e[1..], &Limits::default()).unwrap();
        assert_eq!(f, g);
    }

    #[test]
    fn every_frame_round_trips() {
        let id = NodeId::new(3, 0xabc);
        let op = Op {
            id,
            deps: [NodeId::new(2, 0xabc)].into_iter().collect(),
            payload: Payload::Create {
                kind: Kind::File,
                parent: ROOT,
                name: "a b%\tc".into(),
            },
        };
        let b = BlobId::of(b"x");
        rt(Frame::Hello(Hello::new(ReplicaId(7), true)));
        rt(Frame::Have { frontier: vec![id] });
        rt(Frame::Have { frontier: vec![] });
        rt(Frame::Ops {
            frontier: vec![id],
            done: true,
            ops: vec![op.clone(), op],
        });
        rt(Frame::Ops {
            frontier: vec![],
            done: false,
            ops: vec![],
        });
        rt(Frame::Need(vec![id]));
        let mut nb = vec![(BlobId::empty(), 0), (b, 5)];
        nb.sort();
        rt(Frame::NeedBlobs(nb));
        rt(Frame::NeedBlobs(vec![]));
        rt(Frame::BlobBegin {
            id: b,
            size: 9,
            offset: 2,
        });
        rt(Frame::chunk(b"hello".to_vec()));
        rt(Frame::BlobEnd(b));
        rt(Frame::NoBlob(b));
        rt(Frame::Checkpoints(vec![Checkpoint::from_canonical(
            vec![id],
            "msg with space",
            ReplicaId(1),
            Some("n".into()),
        )]));
        rt(Frame::Checkpoints(vec![]));
        rt(Frame::Done("meta".into()));
        rt(Frame::Committed(vec![id]));
        rt(Frame::Error {
            code: "limit".into(),
            message: "too big: x".into(),
            fatal: true,
        });
        rt(Frame::Progress {
            phase: "ops".into(),
            done: 3,
            total: None,
        });
        rt(Frame::Bye);
    }

    #[test]
    fn hello_floors_and_caps() {
        assert!(Limits::default().check().is_ok());
        for l in [
            Limits {
                frame: 1,
                ..Limits::default()
            },
            Limits {
                chunk: 0,
                ..Limits::default()
            },
            Limits {
                ops: 0,
                ..Limits::default()
            },
            Limits {
                frame: FRAME_CAP + 1,
                ..Limits::default()
            },
            Limits {
                chunk: CHUNK_FLOOR - 1,
                ..Limits::default()
            },
            Limits {
                frame: FRAME_FLOOR,
                chunk: FRAME_FLOOR - 32,
                ops: 1,
            },
            Limits {
                ops: OPS_CAP + 1,
                ..Limits::default()
            },
        ] {
            assert!(l.check().is_err(), "{l:?}");
        }
        assert!(Limits {
            frame: FRAME_FLOOR,
            chunk: CHUNK_FLOOR,
            ops: 1
        }
        .check()
        .is_ok());
    }

    #[test]
    fn malformed_frames_are_protocol_errors() {
        let l = Limits::default();
        for (t, p) in [
            (0x7fu8, &b""[..]),
            (HAVE, b"x -"),
            (OPS, b"- 2"),
            (OPS, b"- 1\nO nonsense"),
            (NEED_BLOBS, b"zz"),
            (DONE, b"other"),
            (BYE, b"x"),
            (HELLO, b"proto 1 1"),
            (ERROR, b"BAD s 1"),
            (HAVE, &[0xff, 0xfe][..]),
        ] {
            let e = decode(t, p, &l).unwrap_err();
            assert!(
                matches!(e.code(), Code::Protocol | Code::BadBlobId),
                "{t:#x} {p:?}: {e}"
            );
        }
        assert_eq!(
            decode(REF_MOVES, b"", &l).unwrap_err().code(),
            Code::Unsupported
        );
        let big = format!(
            "- 0{}",
            "\nO 1.00000000000000000000000000000001 - D 0.00000000000000000000000000000000"
                .repeat(1025)
        );
        assert_eq!(
            decode(OPS, big.as_bytes(), &l).unwrap_err().code(),
            Code::Limit
        );
        let unsorted = format!("{},{}", "b".repeat(64), "a".repeat(64));
        assert_eq!(
            decode(NEED_BLOBS, unsorted.as_bytes(), &l)
                .unwrap_err()
                .code(),
            Code::Protocol
        );
        let chunk = vec![0u8; l.chunk as usize + 33];
        assert_eq!(decode(CHUNK, &chunk, &l).unwrap_err().code(), Code::Limit);
    }

    #[test]
    fn oversized_length_is_refused_before_reading_the_payload() {
        let (mut a, mut b, _) = pair();
        // Only the 4-byte length arrives; the payload is never sent.
        a.tx.write_all(&(Limits::default().frame + 1).to_be_bytes())
            .unwrap();
        let e = b.recv(Duration::from_secs(1)).unwrap_err();
        assert_eq!(e.code(), Code::Limit);
        assert_eq!(b.bytes_in.load(Ordering::Relaxed), 4);
        a.tx.write_all(&0u32.to_be_bytes()).unwrap();
        assert_eq!(
            b.recv(Duration::from_secs(1)).unwrap_err().code(),
            Code::Protocol
        );
    }

    #[test]
    fn pair_delivers_cuts_and_times_out() {
        let (mut a, mut b, cut) = pair();
        a.send_magic().unwrap();
        b.recv_magic(Duration::from_secs(1)).unwrap();
        a.cut_after(2);
        a.send(&Frame::Bye).unwrap();
        a.send(&Frame::Done("ops".into())).unwrap();
        assert!(cut.is_cut());
        assert!(a.send(&Frame::Bye).is_err());
        assert!(b.send(&Frame::Bye).is_err());
        assert_eq!(b.recv(Duration::from_secs(1)).unwrap(), Frame::Bye);
        assert_eq!(
            b.recv(Duration::from_secs(1)).unwrap(),
            Frame::Done("ops".into())
        );
        assert_eq!(b.recv(Duration::from_secs(1)).unwrap_err().code(), Code::Io);
        let (_c, mut d, _) = pair();
        assert_eq!(
            d.recv(Duration::from_millis(20)).unwrap_err().code(),
            Code::Timeout
        );
        let (mut e, mut f, _) = pair();
        e.tx.write_all(b"SSH-2.0-").unwrap();
        assert_eq!(
            f.recv_magic(Duration::from_secs(1)).unwrap_err().code(),
            Code::Protocol
        );
    }
}
