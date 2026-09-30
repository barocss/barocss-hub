//! Wire protocol v1 sessions over persisted repositories (Issue #43, Wiki
//! Remote-Sync §5.2.4, §5.4).
//!
//! A session is pull then push over one connection, then meta:
//! 1. `Hello` both ways (limits, floors, protocol range, replica check).
//! 2. Pull: the client sends `Have`; the server sends every `Ops` frame of
//!    `Δ` (§4.3), then serves requests: `NeedBlobs` (streamed blobs),
//!    `Need` (missing deps, one round as §4.4), until `Done ops`. The client
//!    persists and sends `Committed`.
//! 3. Push: the same with roles swapped (skipped for a read-only server).
//! 4. Meta: both send `Checkpoints` (published only) then `Done meta`; each
//!    persists what it received and sends `Committed`.
//! 5. `Bye` both ways.
//!
//! Validation: every received op and blob goes through
//! `SyncReplica::receive`/`receive_blob_held` (§4.5; no wire fast path).
//! The in-session replica is advisory (it decides `NeedBlobs`/`Need`);
//! each persist step reloads the on-disk state under the exclusive lock and
//! *replays* everything the half received, in arrival order, so concurrent
//! sessions merge (R1, R1b). Blobs are streamed to `blobs/rx-*` files,
//! verified by SHA-256 per chunk and per blob, and renamed into place at
//! persist (never re-read). A drop mid-blob keeps the prefix as
//! `blobs/part-<id>` for resume; a failed resumed transfer is retried once
//! from offset 0 without a warning (R6).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hub_core::checkpoint::{Checkpoint, FrontierCheck};
use hub_core::sync::{delta, Have, RejectReason, SyncReplica};
use hub_core::tree::{BlobId, Op, OpId, ReplicaId};
use hub_core::version::Version;
use sha2::{Digest, Sha256};

use crate::store::{self, LockMode};
use crate::sync::{self, Counts, Interrupt};
use crate::wire::{self, groups, Conn, Frame, Hello};
use crate::{Code, Error, Repo};

/// How a side reaches its repository.
pub enum Access {
    /// Locked for the whole session (the client; both ends of `file://`).
    Held(Box<Repo>),
    /// `.hub/` path: locked only per step (`serve`, §5.4 R1).
    Path(PathBuf),
}

/// What was received in one half, in arrival order (replayed at persist).
enum Recv {
    Ops(Vec<Op>),
    Blob {
        id: BlobId,
        file: Option<PathBuf>,
        ok: bool,
        len: u64,
    },
    Ckpts(Vec<Checkpoint>),
}

/// One end of a session.
pub struct Side {
    pub access: Access,
    pub dir: PathBuf,
    pub id: ReplicaId,
    sr: SyncReplica,
    pub read_only: bool,
    pub stop: Option<Interrupt>,
    pub appended: Arc<AtomicUsize>,
    pub max_blob: u64,
    /// Blob bytes and blobs received (Issue #34 stats).
    pub blob_bytes_in: u64,
    pub blobs_in: usize,
    /// Blobs streamed to the peer.
    pub blobs_sent: usize,
    pub got: Counts,
}

/// What a finished client session reports.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub received: Counts,
    pub sent: Counts,
    pub read_only: bool,
    /// Our head frontier equals the remote's after the session.
    pub same_head: bool,
    pub remote_replica: Option<ReplicaId>,
}

fn lock_wait() -> Duration {
    wire::step_timeout()
}

fn perr(m: impl Into<String>) -> Error {
    Error::new(Code::Protocol, m)
}

fn remote_err(code: &str, message: &str) -> Error {
    Error::remote(wire::code_of(code), format!("remote: {message}"))
}

fn rx_name(dir: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.join(format!(
        "rx-{}-{nanos}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

fn part_name(dir: &Path, id: &BlobId) -> PathBuf {
    dir.join(format!("part-{}", id.hex()))
}

/// Remove `rx-*` files older than a day (crashed sessions) and `part-*`
/// files whose blob exists or that are older than 7 days (§5.2.4).
pub fn sweep_wire_files(bdir: &Path) {
    let Ok(rd) = fs::read_dir(bdir) else { return };
    let now = SystemTime::now();
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        let age = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .unwrap_or_default();
        let old = |d: u64| age > Duration::from_secs(d * 86_400);
        let drop = if n.starts_with("rx-") {
            old(1)
        } else if let Some(h) = n.strip_prefix("part-") {
            old(7) || bdir.join(h).exists()
        } else {
            false
        };
        if drop {
            let _ = fs::remove_file(e.path());
        }
    }
}

fn dups(s: &SyncReplica) -> usize {
    s.warnings()
        .iter()
        .filter(|w| w.reason == RejectReason::DuplicateReplica)
        .count()
}

fn dup_error(why: &str) -> Error {
    Error::new(
        Code::DuplicateReplica,
        format!(
            "sync aborted: {why} (duplicate replica id, D-022). One repository is probably a \
             copy of the other's .hub/; run `barocss-hub reid` in one of them, then sync again"
        ),
    )
}

impl Side {
    /// Load a side. A `Path` side reads under a shared lock that is
    /// released before the session goes on.
    pub fn new(access: Access, read_only: bool) -> Result<Side, Error> {
        let (dir, id, sr) = match &access {
            Access::Held(r) => (r.dir.clone(), r.replica_id, sync::load(r)?),
            Access::Path(d) => {
                let r = Repo::open_dir_opts(d.clone(), LockMode::Shared, Some(lock_wait()))?;
                let sr = sync::load_opts(&r, false)?;
                (r.dir.clone(), r.replica_id, sr)
            }
        };
        Ok(Side {
            access,
            dir,
            id,
            sr,
            read_only,
            stop: None,
            appended: Arc::new(AtomicUsize::new(0)),
            max_blob: std::env::var("HUB_TEST_MAX_BLOB_SIZE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(wire::MAX_BLOB_SIZE),
            blob_bytes_in: 0,
            blobs_in: 0,
            blobs_sent: 0,
            got: Counts::default(),
        })
    }

    pub fn replica(&self) -> &SyncReplica {
        &self.sr
    }

    pub fn repo(&self) -> Option<&Repo> {
        match &self.access {
            Access::Held(r) => Some(r),
            Access::Path(_) => None,
        }
    }

    fn bdir(&self) -> PathBuf {
        self.dir.join("blobs")
    }

    /// A persisted op-level DuplicateReplica warning naming `peer` stops
    /// syncs with it until `reid` (D-022, §5.8).
    fn refuses(&self, peer: ReplicaId) -> bool {
        self.sr
            .warnings()
            .iter()
            .any(|w| w.reason == RejectReason::DuplicateReplica && w.peer == peer)
    }

    // ------------------------------------------------------------ persist

    /// Reload under the exclusive lock, replay `log`, write (A.1 order).
    fn persist(&mut self, log: Vec<Recv>, peer: ReplicaId) -> Result<Counts, Error> {
        let mut opened;
        let repo: &mut Repo = match &mut self.access {
            Access::Held(r) => r,
            Access::Path(d) => {
                opened = Repo::open_dir_opts(d.clone(), LockMode::Exclusive, Some(lock_wait()))?;
                &mut opened
            }
        };
        let mut s = sync::load(repo)?;
        let d0 = dups(&s);
        let mut files: HashMap<BlobId, PathBuf> = HashMap::new();
        let mut ckpts: Vec<Checkpoint> = Vec::new();
        for item in log {
            match item {
                Recv::Ops(v) => {
                    for o in v {
                        s.receive(peer, o);
                    }
                }
                Recv::Blob { id, file, ok, len } => {
                    s.receive_blob_held(peer, id, ok, len);
                    if let Some(f) = file {
                        if let Some(old) = files.insert(id, f) {
                            let _ = fs::remove_file(old);
                        }
                    }
                }
                Recv::Ckpts(v) => ckpts.extend(v),
            }
        }
        if !ckpts.is_empty() {
            let canon: Vec<Checkpoint> = {
                let check = FrontierCheck::new(s.store());
                ckpts
                    .into_iter()
                    .map(|c| {
                        Checkpoint::from_frontier_checked(
                            &check,
                            c.frontier().iter().copied(),
                            c.message.clone(),
                            c.author,
                            c.note.clone(),
                        )
                        .unwrap_or(c)
                    })
                    .collect()
            };
            for c in canon {
                s.publish(c);
            }
        }
        let cleanup = |files: &HashMap<BlobId, PathBuf>| {
            for f in files.values() {
                let _ = fs::remove_file(f);
            }
        };
        if dups(&s) > d0 {
            // Keep the warnings (D-021), persist no ops (D-022).
            sync::persist_warnings(repo, &s)?;
            cleanup(&files);
            return Err(dup_error(
                "received ops authored under our own replica id that we never authored",
            ));
        }
        let mut n = self.appended.load(Ordering::SeqCst);
        let r = sync::persist_with(repo, &s, &files, &mut n, self.stop);
        self.appended.store(n, Ordering::SeqCst);
        cleanup(&files);
        let c = r?;
        if let Some(w) = repo.maybe_snapshot() {
            eprintln!("warning: {w}");
        }
        self.sr = s;
        Ok(c)
    }

    // ------------------------------------------------------------ answerer

    /// Send `Δ` for `frontier` as `Ops` frames, then serve `NeedBlobs` and
    /// `Need` until `Done ops`.
    fn answer(&mut self, conn: &mut Conn, frontier: Vec<OpId>) -> Result<(), Error> {
        let d = delta(
            self.sr.store(),
            &Have {
                frontier,
                vv_hint: None,
            },
        );
        self.send_ops(conn, d)?;
        loop {
            match conn.recv(wire::step_timeout())? {
                Frame::NeedBlobs(v) => {
                    for (id, off) in v {
                        self.stream_blob(conn, id, off)?;
                    }
                }
                Frame::Need(ids) => {
                    let ops: BTreeMap<OpId, Op> = ids
                        .iter()
                        .filter_map(|i| self.sr.store().get(i).map(|o| (*i, o.clone())))
                        .collect();
                    self.send_ops(conn, ops.into_values().collect())?;
                }
                Frame::Done(p) if p == "ops" => return Ok(()),
                f => self.other(f)?,
            }
        }
    }

    /// Frames acceptable anywhere: `Progress`, a non-fatal `Error`.
    /// Anything else here is `protocol`, or `forbidden` for a write frame
    /// sent to a read-only server (D-043).
    fn other(&self, f: Frame) -> Result<(), Error> {
        match f {
            Frame::Progress { .. } => Ok(()),
            Frame::Error { fatal: false, .. } => Ok(()),
            Frame::Error { code, message, .. } => Err(remote_err(&code, &message)),
            f if self.read_only
                && matches!(
                    f,
                    Frame::Ops { .. }
                        | Frame::BlobBegin { .. }
                        | Frame::Chunk { .. }
                        | Frame::Checkpoints(_)
                ) =>
            {
                Err(Error::new(
                    Code::Forbidden,
                    format!("{} refused: this server is read-only", f.name()),
                ))
            }
            f => Err(perr(format!("unexpected {} frame", f.name()))),
        }
    }

    fn send_ops(&self, conn: &mut Conn, ops: Vec<Op>) -> Result<(), Error> {
        let frontier = self.sr.frontier();
        let head = wire::fmt_ids(&frontier).len() + 8;
        let max = (conn.theirs.frame as usize).saturating_sub(head);
        let bodies: Vec<(Op, usize)> = ops
            .into_iter()
            .map(|o| {
                let n = store::encode_op(&o).len();
                (o, n)
            })
            .collect();
        if let Some((o, _)) = bodies.iter().find(|(_, n)| *n + 1 > max) {
            return Err(Error::new(
                Code::Limit,
                format!(
                    "op {} is larger than the remote accepts; run with a larger frame limit",
                    store::fmt_id(o.id)
                ),
            ));
        }
        let mut gs = groups(bodies, conn.theirs.ops as usize, max, |(_, n)| *n);
        if gs.is_empty() {
            gs.push(Vec::new());
        }
        let n = gs.len();
        for (i, g) in gs.into_iter().enumerate() {
            conn.send(&Frame::Ops {
                frontier: frontier.clone(),
                done: i + 1 == n,
                ops: g.into_iter().map(|(o, _)| o).collect(),
            })?;
        }
        Ok(())
    }

    fn stream_blob(&mut self, conn: &mut Conn, id: BlobId, off: u64) -> Result<(), Error> {
        if id == BlobId::empty() {
            if off != 0 {
                return conn.send(&Frame::NoBlob(id));
            }
            conn.send(&Frame::BlobBegin {
                id,
                size: 0,
                offset: 0,
            })?;
            self.blobs_sent += 1;
            return conn.send(&Frame::BlobEnd(id));
        }
        let mut f = match File::open(self.bdir().join(id.hex())) {
            Ok(f) => f,
            Err(_) => return conn.send(&Frame::NoBlob(id)),
        };
        let size = f.metadata()?.len();
        if off > size {
            return conn.send(&Frame::NoBlob(id));
        }
        conn.send(&Frame::BlobBegin {
            id,
            size,
            offset: off,
        })?;
        f.seek(SeekFrom::Start(off))?;
        let cs = conn.theirs.chunk.min(256 * 1024) as usize;
        let mut left = size - off;
        let mut buf = vec![0u8; cs];
        // Our own copy is checked as it streams (the prefix before `off`
        // too): a damaged local blob ends the session with `corrupt`
        // before `BlobEnd`, rather than blaming the receiver's peer.
        let mut h = Sha256::new();
        if off > 0 {
            let mut g = File::open(self.bdir().join(id.hex()))?;
            let mut left = off;
            while left > 0 {
                let n = (left as usize).min(cs);
                g.read_exact(&mut buf[..n])?;
                h.update(&buf[..n]);
                left -= n as u64;
            }
        }
        while left > 0 {
            let n = (left as usize).min(cs);
            f.read_exact(&mut buf[..n])?;
            h.update(&buf[..n]);
            left -= n as u64;
            if left == 0 && BlobId(h.clone().finalize().into()) != id {
                return Err(Error::new(
                    Code::Corrupt,
                    format!(
                        "cannot sync: corrupt blob {}: content hash does not match ({})",
                        id.hex(),
                        self.bdir().join(id.hex()).display()
                    ),
                ));
            }
            conn.send(&Frame::chunk(buf[..n].to_vec()))?;
        }
        self.blobs_sent += 1;
        conn.send(&Frame::BlobEnd(id))
    }

    // ------------------------------------------------------------ requester

    /// Request `Δ` from the peer and receive it (one half, §5.2.4).
    fn request(&mut self, conn: &mut Conn, peer: ReplicaId, resume: bool) -> Result<Vec<Recv>, Error> {
        let mut log = Vec::new();
        let mut requested: BTreeSet<BlobId> = BTreeSet::new();
        let mut outstanding: BTreeMap<BlobId, u64> = BTreeMap::new();
        let mut retried: BTreeSet<BlobId> = BTreeSet::new();
        conn.send(&Frame::Have {
            frontier: self.sr.frontier(),
        })?;
        let last = self.recv_ops(conn, peer, resume, &mut log, &mut requested, &mut outstanding)?;
        self.recv_blobs(conn, peer, &mut log, &mut outstanding, &mut retried)?;
        let need = self.sr.need(&last).ids;
        if !need.is_empty() {
            let max = conn.theirs.frame as usize - 1;
            let gs = groups(need, wire::MAX_LIST, max, |i| store::fmt_id(*i).len());
            for g in &gs {
                conn.send(&Frame::Need(g.clone()))?;
            }
            for _ in &gs {
                self.recv_ops(conn, peer, resume, &mut log, &mut requested, &mut outstanding)?;
            }
            self.recv_blobs(conn, peer, &mut log, &mut outstanding, &mut retried)?;
        }
        conn.send(&Frame::Done("ops".into()))?;
        Ok(log)
    }

    /// Read `Ops` frames up to `done`; after each, request the blobs we lack.
    fn recv_ops(
        &mut self,
        conn: &mut Conn,
        peer: ReplicaId,
        resume: bool,
        log: &mut Vec<Recv>,
        requested: &mut BTreeSet<BlobId>,
        outstanding: &mut BTreeMap<BlobId, u64>,
    ) -> Result<Vec<OpId>, Error> {
        loop {
            match conn.recv(wire::step_timeout())? {
                Frame::Ops {
                    frontier,
                    done,
                    ops,
                } => {
                    for o in &ops {
                        self.sr.receive(peer, o.clone());
                    }
                    let mut want: BTreeMap<BlobId, u64> = BTreeMap::new();
                    for b in self.sr.lacking_blobs(&ops) {
                        if requested.insert(b) {
                            let off = if resume {
                                fs::metadata(part_name(&self.bdir(), &b))
                                    .map(|m| m.len())
                                    .unwrap_or(0)
                            } else {
                                0
                            };
                            want.insert(b, off);
                        }
                    }
                    log.push(Recv::Ops(ops));
                    self.need_blobs(conn, want, outstanding)?;
                    if done {
                        return Ok(frontier);
                    }
                }
                f => self.other(f)?,
            }
        }
    }

    fn need_blobs(
        &self,
        conn: &mut Conn,
        want: BTreeMap<BlobId, u64>,
        outstanding: &mut BTreeMap<BlobId, u64>,
    ) -> Result<(), Error> {
        let v: Vec<(BlobId, u64)> = want.into_iter().collect();
        outstanding.extend(v.iter().copied());
        let gs = groups(v, wire::MAX_LIST, conn.theirs.frame as usize - 1, |_| 86);
        if gs.is_empty() {
            return conn.send(&Frame::NeedBlobs(Vec::new()));
        }
        for g in gs {
            conn.send(&Frame::NeedBlobs(g))?;
        }
        Ok(())
    }

    /// Receive the streams of every outstanding blob (and any retries).
    fn recv_blobs(
        &mut self,
        conn: &mut Conn,
        peer: ReplicaId,
        log: &mut Vec<Recv>,
        outstanding: &mut BTreeMap<BlobId, u64>,
        retried: &mut BTreeSet<BlobId>,
    ) -> Result<(), Error> {
        let bdir = self.bdir();
        let mut retry: BTreeMap<BlobId, u64> = BTreeMap::new();
        loop {
            if outstanding.is_empty() {
                if retry.is_empty() {
                    return Ok(());
                }
                let r = std::mem::take(&mut retry);
                self.need_blobs(conn, r, outstanding)?;
                continue;
            }
            match conn.recv(wire::step_timeout())? {
                Frame::NoBlob(id) => {
                    if outstanding.remove(&id).is_none() {
                        return Err(perr("NoBlob for a blob that was not requested"));
                    }
                }
                Frame::BlobBegin { id, size, offset } => {
                    let Some(want) = outstanding.remove(&id) else {
                        return Err(perr(format!("unsolicited blob {}", id.hex())));
                    };
                    if offset != want || offset > size {
                        return Err(perr(format!("bad offset for blob {}", id.hex())));
                    }
                    if size > self.max_blob {
                        conn.send_error(
                            &Error::new(
                                Code::Limit,
                                format!("blob {} of {size} bytes exceeds the limit", id.hex()),
                            ),
                            false,
                        );
                        self.skip_blob(conn, id, size - offset)?;
                        continue;
                    }
                    fs::create_dir_all(&bdir)?;
                    let (path, ok, n) = self.recv_one(conn, &bdir, id, size, offset)?;
                    if !ok && offset > 0 && retried.insert(id) {
                        let _ = fs::remove_file(&path);
                        retry.insert(id, 0);
                        continue;
                    }
                    if !ok {
                        let _ = fs::remove_file(&path);
                    }
                    self.sr.receive_blob_held(peer, id, ok, n);
                    self.blob_bytes_in += n;
                    self.blobs_in += 1;
                    log.push(Recv::Blob {
                        id,
                        file: ok.then_some(path),
                        ok,
                        len: n,
                    });
                }
                f => self.other(f)?,
            }
        }
    }

    fn skip_blob(&self, conn: &mut Conn, id: BlobId, left: u64) -> Result<(), Error> {
        let mut got = 0u64;
        loop {
            match conn.recv(wire::step_timeout())? {
                Frame::Chunk { data, .. } => {
                    got += data.len() as u64;
                    if got > left {
                        return Err(perr("chunks past the blob size"));
                    }
                }
                Frame::BlobEnd(b) if b == id && got == left => return Ok(()),
                Frame::BlobEnd(_) => return Err(perr("BlobEnd before the blob size")),
                f => self.other(f)?,
            }
        }
    }

    /// Stream one blob into an `rx-*` file. Returns the file, whether the
    /// whole content hashes to `id`, and the bytes transferred.
    fn recv_one(
        &self,
        conn: &mut Conn,
        bdir: &Path,
        id: BlobId,
        size: u64,
        offset: u64,
    ) -> Result<(PathBuf, bool, u64), Error> {
        let path = rx_name(bdir);
        let mut h = Sha256::new();
        let mut f = if offset > 0 {
            // Claim the part atomically (one session wins), check its
            // length and re-hash the kept prefix.
            fs::rename(part_name(bdir, &id), &path)
                .map_err(|_| perr(format!("no partial blob {} to resume", id.hex())))?;
            let mut f = OpenOptions::new().read(true).write(true).open(&path)?;
            if f.metadata()?.len() < offset {
                let _ = fs::remove_file(&path);
                return Err(perr("partial blob shorter than the resume offset"));
            }
            f.set_len(offset)?;
            let mut buf = vec![0u8; 64 * 1024];
            let mut left = offset;
            while left > 0 {
                let n = (left as usize).min(buf.len());
                f.read_exact(&mut buf[..n])?;
                h.update(&buf[..n]);
                left -= n as u64;
            }
            f.seek(SeekFrom::End(0))?;
            f
        } else {
            File::create(&path)?
        };
        let mut got = offset;
        let res: Result<(), Error> = (|| loop {
            match conn.recv(wire::step_timeout())? {
                Frame::Chunk { data, sha } => {
                    let d: [u8; 32] = Sha256::digest(&data).into();
                    if d != sha {
                        return Err(Error::new(
                            Code::Corrupt,
                            format!("chunk of blob {} damaged in transit", id.hex()),
                        ));
                    }
                    if got + data.len() as u64 > size {
                        return Err(perr("chunks past the blob size"));
                    }
                    f.write_all(&data)?;
                    h.update(&data);
                    got += data.len() as u64;
                }
                Frame::BlobEnd(b) => {
                    if b != id || got != size {
                        return Err(perr("BlobEnd before the blob size"));
                    }
                    return Ok(());
                }
                fr => self.other(fr)?,
            }
        })();
        if let Err(e) = res {
            drop(f);
            if matches!(e.code(), Code::Io | Code::Timeout) && got > 0 {
                let _ = fs::rename(&path, part_name(bdir, &id));
            } else {
                let _ = fs::remove_file(&path);
            }
            return Err(e);
        }
        crate::durable::flush(&f)?;
        let ok = BlobId(h.finalize().into()) == id;
        Ok((path, ok, size - offset))
    }

    // ------------------------------------------------------------ meta

    fn send_checkpoints(&self, conn: &mut Conn) -> Result<Vec<Checkpoint>, Error> {
        let recs = self.sr.checkpoints_msg().records;
        let max = conn.theirs.frame as usize - 1;
        for g in groups(recs.clone(), usize::MAX, max, |c| store::encode_ckpt(c).len()) {
            conn.send(&Frame::Checkpoints(g))?;
        }
        Ok(recs)
    }

    fn recv_meta(&self, conn: &mut Conn) -> Result<Vec<Checkpoint>, Error> {
        let mut got = Vec::new();
        loop {
            match conn.recv(wire::step_timeout())? {
                Frame::Checkpoints(v) if !self.read_only => got.extend(v),
                Frame::Done(p) if p == "meta" => return Ok(got),
                f => self.other(f)?,
            }
        }
    }

    fn expect_committed(&self, conn: &mut Conn) -> Result<Vec<OpId>, Error> {
        loop {
            match conn.recv(wire::step_timeout())? {
                Frame::Committed(f) => return Ok(f),
                f => self.other(f)?,
            }
        }
    }

    fn bye(&self, conn: &mut Conn) -> Result<(), Error> {
        conn.send(&Frame::Bye)?;
        loop {
            match conn.recv(wire::step_timeout())? {
                Frame::Bye => return Ok(()),
                f => self.other(f)?,
            }
        }
    }
}

/// `Hello` both ways (§5.2.3). Returns the peer's `Hello`.
pub fn handshake(conn: &mut Conn, side: &Side) -> Result<Hello, Error> {
    conn.send_magic()?;
    conn.send(&Frame::Hello(Hello::new(side.id, side.read_only)))?;
    conn.recv_magic(wire::hello_timeout())?;
    let h = match conn.recv(wire::hello_timeout())? {
        Frame::Hello(h) => h,
        Frame::Error { code, message, .. } => return Err(remote_err(&code, &message)),
        f => return Err(perr(format!("expected Hello, got {}", f.name()))),
    };
    if let Err(why) = h.limits.check() {
        return Err(perr(format!("remote Hello: {why}")));
    }
    let lo = h.proto_min.max(wire::PROTO_MIN);
    let hi = h.proto_max.min(wire::PROTO_MAX);
    if h.proto_min > h.proto_max || lo > hi {
        return Err(Error::new(
            Code::VersionMismatch,
            format!(
                "no common wire protocol: local {}..{}, remote {}..{}",
                wire::PROTO_MIN,
                wire::PROTO_MAX,
                h.proto_min,
                h.proto_max
            ),
        ));
    }
    for f in ["blobs-chunked", "checkpoints"] {
        if !h.has(f) {
            return Err(Error::new(
                Code::Unsupported,
                format!("remote lacks mandatory feature {f}"),
            ));
        }
    }
    if h.replica == side.id {
        return Err(dup_error(&format!(
            "the remote has our replica id {:032x}",
            side.id.0
        )));
    }
    if side.refuses(h.replica) {
        return Err(dup_error(&format!(
            "a saved duplicate-replica warning names the remote {:032x}",
            h.replica.0
        )));
    }
    conn.theirs = h.limits;
    Ok(h)
}

/// Send our error to the peer (unless it came from the peer) and return it.
fn fail<T>(conn: &mut Conn, r: Result<T, Error>) -> Result<T, Error> {
    if let Err(e) = &r {
        if !e.is_remote() && !matches!(e.code(), Code::Io | Code::Timeout) {
            conn.send_error(e, true);
        }
    }
    r
}

fn count(store: &hub_core::version::Store, f: &[OpId]) -> Option<usize> {
    Version::downclose(store, f.iter().copied())
        .ok()
        .map(|v| v.len())
}

/// The client (connecting) end.
pub fn client(conn: &mut Conn, side: &mut Side) -> Result<Report, Error> {
    let r = client_inner(conn, side);
    fail(conn, r)
}

fn client_inner(conn: &mut Conn, side: &mut Side) -> Result<Report, Error> {
    let h = handshake(conn, side)?;
    let peer = h.replica;
    let ro = h.has(wire::F_READ_ONLY);
    let resume = h.has("resume");
    let mut rep = Report {
        read_only: ro,
        remote_replica: Some(peer),
        ..Report::default()
    };
    // Pull.
    let log = side.request(conn, peer, resume)?;
    rep.received = side.persist(log, peer)?;
    conn.send(&Frame::Committed(side.sr.frontier()))?;
    // Push.
    if !ro {
        let before = loop {
            match conn.recv(wire::step_timeout())? {
                Frame::Have { frontier } => break frontier,
                f => side.other(f)?,
            }
        };
        let sent0 = side.blobs_sent;
        side.answer(conn, before.clone())?;
        let after = side.expect_committed(conn)?;
        let st = side.sr.store();
        rep.sent.ops = match (count(st, &before), count(st, &after)) {
            (Some(a), Some(b)) => b.saturating_sub(a),
            _ => delta(
                st,
                &Have {
                    frontier: before,
                    vv_hint: None,
                },
            )
            .len(),
        };
        rep.sent.blobs = side.blobs_sent - sent0;
    }
    // Meta.
    let mine = if ro {
        Vec::new()
    } else {
        side.send_checkpoints(conn)?
    };
    conn.send(&Frame::Done("meta".into()))?;
    let theirs = side.recv_meta(conn)?;
    rep.sent.checkpoints = mine.iter().filter(|c| !theirs.contains(c)).count();
    let got = side.persist(vec![Recv::Ckpts(theirs)], peer)?;
    rep.received.checkpoints = got.checkpoints;
    conn.send(&Frame::Committed(side.sr.frontier()))?;
    let remote_head = side.expect_committed(conn)?;
    rep.same_head = remote_head == side.sr.frontier();
    side.bye(conn)?;
    Ok(rep)
}

/// The server (`serve --stdio`, or the far end of `file://`).
pub fn server(conn: &mut Conn, side: &mut Side) -> Result<(), Error> {
    let r = server_inner(conn, side);
    fail(conn, r)
}

fn server_inner(conn: &mut Conn, side: &mut Side) -> Result<(), Error> {
    let h = handshake(conn, side)?;
    let peer = h.replica;
    let resume = h.has("resume");
    // Pull half: we answer.
    let frontier = loop {
        match conn.recv(wire::step_timeout())? {
            Frame::Have { frontier } => break frontier,
            f => side.other(f)?,
        }
    };
    side.answer(conn, frontier)?;
    side.expect_committed(conn)?;
    // Push half: we request (never on a read-only server).
    if !side.read_only {
        let log = side.request(conn, peer, resume)?;
        side.got = side.persist(log, peer)?;
        conn.send(&Frame::Committed(side.sr.frontier()))?;
    }
    side.send_checkpoints(conn)?;
    conn.send(&Frame::Done("meta".into()))?;
    let got = side.recv_meta(conn)?;
    if !got.is_empty() && !side.read_only {
        side.got.checkpoints = side.persist(vec![Recv::Ckpts(got)], peer)?.checkpoints;
    }
    conn.send(&Frame::Committed(side.sr.frontier()))?;
    side.expect_committed(conn)?;
    side.bye(conn)
}

/// Pending ops of a side (for the `file://` report).
pub fn pending_len(side: &Side) -> usize {
    side.sr.pending_len()
}

#[allow(dead_code)]
fn _assert_send() {
    fn is_send<T: Send>() {}
    is_send::<Side>();
}
