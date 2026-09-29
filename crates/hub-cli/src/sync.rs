//! `hub sync <path>` between two local repositories (Issue #22).
//!
//! Both `.hub/` directories are opened in one process, each under its own
//! exclusive `flock`, taken in a fixed order (by canonical `.hub/` path) so
//! two opposite syncs (`A: hub sync B` and `B: hub sync A`) cannot deadlock.
//! Each side is loaded into a `hub_core::sync::SyncReplica` and the §4.2
//! messages run in memory (`hub_core::sync::sync`): pull both ways, then the
//! published checkpoints. Nothing is persisted until both pulls finish.
//!
//! Persist order per side (crash-safe, any prefix is a valid repo):
//! 1. blobs of every new applied or pending `SetContent` (verified by the
//!    core on receive and again by `Blobs::put`);
//! 2. new applied ops appended to `ops.log` in `<` order (a dep is `<` its
//!    op, so every prefix is causally closed and replays);
//! 3. `pending.log` and `warnings.log` rewritten atomically;
//! 4. new published checkpoint records appended to `published.log`.
//!
//! An interrupted sync leaves a valid prefix; a rerun sends the rest.
//!
//! Sync is log-only: it never touches the working directory or the index.
//! After a sync that received ops, `hub status` reports the working
//! directory behind head and `hub checkout` brings it up to date.
//!
//! Files (all optional; absent = empty, so pre-#22 repos open unchanged),
//! same framing as `ops.log` (`<body>\t<crc>\n` after a header line):
//! - `pending.log`   `barocss-hub/pending/v0`: `P <peer> <op body>`
//! - `warnings.log`  `barocss-hub/warnings/v0`:
//!   `W <peer> <author|-> <opid|-> <reason> <first_seen> <str detail>`
//! - `published.log` `barocss-hub/published/v0`: checkpoint `K ...` records.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use hub_core::checkpoint::{Checkpoint, VersionId};
use hub_core::sync::{self as core, PeerId, RejectReason, RejectWarning, SyncReplica};
use hub_core::tree::{BlobStore, Op, Payload, ReplicaId};
use hub_core::version::Version;

use crate::store::{self, LockMode};
use crate::{err, Error, Repo, HUB_DIR};

pub const PENDING_HEADER: &str = "barocss-hub/pending/v0";
pub const WARNINGS_HEADER: &str = "barocss-hub/warnings/v0";
pub const PUBLISHED_HEADER: &str = "barocss-hub/published/v0";

/// Test-only fault injection: abort the process after this many ops have
/// been appended (across both repos) during a sync.
#[doc(hidden)]
pub const CRASH_ENV: &str = "HUB_TEST_CRASH_IN_SYNC";

/// Ops per `Ops` batch.
const BATCH: usize = 256;

fn read_opt(path: &Path, header: &str) -> Result<Vec<String>, Error> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(store::read_log(path, header)?.records)
}

fn rewrite(path: &Path, header: &str, bodies: &[String]) -> Result<(), Error> {
    let mut s = format!("{header}\n");
    for b in bodies {
        s.push_str(&store::frame(b));
    }
    Ok(store::write_atomic(path, s.as_bytes())?)
}

fn corrupt(path: &Path, i: usize, why: String) -> Error {
    Error(format!("corrupt {} line {}: {why}", path.display(), i + 2))
}

fn hex_rid(t: &str) -> Result<ReplicaId, String> {
    if t.len() != 32 {
        return Err("bad replica id".into());
    }
    u128::from_str_radix(t, 16)
        .map(ReplicaId)
        .map_err(|_| "bad replica id".into())
}

pub fn read_pending(dir: &Path) -> Result<Vec<(Op, PeerId)>, Error> {
    let p = dir.join("pending.log");
    read_opt(&p, PENDING_HEADER)?
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let r = (|| {
                let rest = b.strip_prefix("P ").ok_or("not a pending record")?;
                let (peer, op) = rest.split_once(' ').ok_or("bad pending record")?;
                Ok::<_, String>((store::decode_op(op)?, hex_rid(peer)?))
            })();
            r.map_err(|why| corrupt(&p, i, why))
        })
        .collect()
}

fn reason_str(r: &RejectReason) -> &'static str {
    match r {
        RejectReason::Equivocation => "equivocation",
        RejectReason::LamportViolation => "lamport-violation",
        RejectReason::CorruptBlob => "corrupt-blob",
        RejectReason::DuplicateReplica => "duplicate-replica",
    }
}

fn encode_warning(w: &RejectWarning) -> String {
    format!(
        "W {:032x} {} {} {} {} {}",
        w.peer.0,
        w.author
            .map(|a| format!("{:032x}", a.0))
            .unwrap_or_else(|| "-".into()),
        w.rejected.map(store::fmt_id).unwrap_or_else(|| "-".into()),
        reason_str(&w.reason),
        w.first_seen,
        store::esc(&w.detail)
    )
}

fn decode_warning(b: &str) -> Result<RejectWarning, String> {
    let t: Vec<&str> = b.split(' ').collect();
    let ["W", peer, author, rejected, reason, first, detail] = t.as_slice() else {
        return Err("not a warning record".into());
    };
    Ok(RejectWarning {
        peer: hex_rid(peer)?,
        author: if *author == "-" {
            None
        } else {
            Some(hex_rid(author)?)
        },
        rejected: if *rejected == "-" {
            None
        } else {
            Some(store::parse_id(rejected)?)
        },
        reason: match *reason {
            "equivocation" => RejectReason::Equivocation,
            "lamport-violation" => RejectReason::LamportViolation,
            "corrupt-blob" => RejectReason::CorruptBlob,
            "duplicate-replica" => RejectReason::DuplicateReplica,
            _ => return Err("bad reason".into()),
        },
        detail: store::unesc(detail)?,
        stuck: Vec::new(),
        first_seen: first.parse().map_err(|_| "bad first_seen")?,
    })
}

pub fn read_warnings(dir: &Path) -> Result<Vec<RejectWarning>, Error> {
    let p = dir.join("warnings.log");
    read_opt(&p, WARNINGS_HEADER)?
        .iter()
        .enumerate()
        .map(|(i, b)| decode_warning(b).map_err(|why| corrupt(&p, i, why)))
        .collect()
}

/// Published checkpoint records (grow-only, §4.6). Only records whose
/// frontier is in the op log are ever written, so all resolve.
pub fn read_published(repo: &Repo) -> Result<Vec<Checkpoint>, Error> {
    let p = repo.dir.join("published.log");
    let mut out: Vec<Checkpoint> = Vec::new();
    for (i, b) in read_opt(&p, PUBLISHED_HEADER)?.iter().enumerate() {
        let c = store::decode_ckpt(b, &repo.store).map_err(|why| corrupt(&p, i, why))?;
        if !out.contains(&c) {
            out.push(c);
        }
    }
    Ok(out)
}

pub fn append_published(repo: &Repo, c: &Checkpoint) -> Result<(), Error> {
    append_published_all(repo, &[c])
}

/// Append several published records with one sync (Issue #29).
pub fn append_published_all(repo: &Repo, cs: &[&Checkpoint]) -> Result<(), Error> {
    if cs.is_empty() {
        return Ok(());
    }
    let p = repo.dir.join("published.log");
    if !p.exists() {
        store::write_atomic(&p, format!("{PUBLISHED_HEADER}\n").as_bytes())?;
    }
    let mut log = store::LogAppender::open(&p)?;
    for c in cs {
        log.write(&store::encode_ckpt(c))?;
    }
    Ok(log.sync()?)
}

/// A core replica without blobs: enough to recompute warnings' `stuck`.
pub fn warnings_view(repo: &Repo) -> Result<(Vec<RejectWarning>, usize), Error> {
    let r = SyncReplica::from_parts(
        repo.replica_id,
        repo.store.clone(),
        BlobStore::new(),
        read_pending(&repo.dir)?,
        read_warnings(&repo.dir)?,
        Vec::new(),
    );
    Ok((r.warnings(), r.pending_len()))
}

pub fn render_warning(w: &RejectWarning) -> String {
    let mut s = format!(
        "{} from peer {}",
        reason_str(&w.reason),
        &format!("{:032x}", w.peer.0)[..8]
    );
    if let Some(r) = w.rejected {
        s.push_str(&format!(" op {}", store::fmt_id(r)));
    }
    s.push_str(&format!(": {}", w.detail));
    if !w.stuck.is_empty() {
        let v: Vec<String> = w.stuck.iter().map(|o| store::fmt_id(*o)).collect();
        s.push_str(&format!("; stuck: {}", v.join(", ")));
    }
    s
}

fn content_blob(o: &Op) -> Option<hub_core::tree::BlobId> {
    match &o.payload {
        Payload::SetContent { blob, .. } => Some(*blob),
        _ => None,
    }
}

fn load(repo: &Repo) -> Result<SyncReplica, Error> {
    let disk = repo.blobs()?;
    disk.sweep_temps()?;
    let mut bs = BlobStore::new();
    for o in repo.store.values() {
        if let Some(b) = content_blob(o) {
            let bytes = disk.get(&b).map_err(|e| {
                Error(format!(
                    "cannot sync {}: {e} (named by op {})",
                    repo.root().display(),
                    store::fmt_id(o.id)
                ))
            })?;
            bs.put(&bytes);
        }
    }
    let pending = read_pending(&repo.dir)?;
    for (o, _) in &pending {
        if let Some(bytes) = content_blob(o).and_then(|b| disk.get(&b).ok()) {
            bs.put(&bytes);
        }
    }
    Ok(SyncReplica::from_parts(
        repo.replica_id,
        repo.store.clone(),
        bs,
        pending,
        read_warnings(&repo.dir)?,
        read_published(repo)?,
    ))
}

struct Counts {
    ops: usize,
    blobs: usize,
    checkpoints: usize,
}

fn persist_warnings(repo: &Repo, s: &SyncReplica) -> Result<(), Error> {
    let ws: Vec<String> = s.warnings().iter().map(encode_warning).collect();
    rewrite(&repo.dir.join("warnings.log"), WARNINGS_HEADER, &ws)
}

/// Where a sync stops early (tests only). `Abort` is the process-level
/// [`CRASH_ENV`] hook; `Return` is its in-process equivalent: the same point
/// (after this many op appends across both repos, before the rest of the
/// persist steps) but reported as an [`INTERRUPTED`] error instead of
/// killing the process, so property tests can interrupt many syncs cheaply.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interrupt {
    Abort(usize),
    Return(usize),
}

/// Error message of an in-process [`Interrupt::Return`].
pub const INTERRUPTED: &str = "sync interrupted (test hook)";

fn persist(
    repo: &mut Repo,
    s: &SyncReplica,
    appended: &mut usize,
    stop: Option<Interrupt>,
) -> Result<Counts, Error> {
    let new_ops: Vec<Op> = s
        .store()
        .values()
        .filter(|o| !repo.store.contains_key(&o.id))
        .cloned()
        .collect();
    let pending = s.pending_ops();
    // 1. Blobs first: an op on disk never names a missing blob.
    let disk = repo.blobs()?;
    let mut nblobs = 0;
    let want: BTreeSet<_> = new_ops
        .iter()
        .chain(pending.iter().map(|(o, _)| o))
        .filter_map(content_blob)
        .collect();
    // Group commit (Issue #29): one barrier and one dir fsync for all.
    let mut batch = disk.batch();
    for b in want {
        if disk.contains(&b) {
            continue;
        }
        if let Some(bytes) = s.blobs().get(&b) {
            batch.add(bytes).map_err(|e| Error(e.to_string()))?;
            nblobs += 1;
        }
    }
    batch.commit().map_err(|e| Error(e.to_string()))?;
    // 2. Ops in `<` order.
    // One batch, so the view is refreshed once (Issue #28).
    let base = *appended;
    let (n, res) = repo.append_ops_with(new_ops.iter().cloned(), |i| match stop {
        Some(Interrupt::Abort(n)) if n == base + i => std::process::abort(),
        Some(Interrupt::Return(n)) if n == base + i => err(INTERRUPTED),
        _ => Ok(()),
    });
    *appended += n;
    res?;
    // 3. Pending buffer and warnings.
    let ps: Vec<String> = pending
        .iter()
        .map(|(o, p)| format!("P {:032x} {}", p.0, store::encode_op(o)))
        .collect();
    rewrite(&repo.dir.join("pending.log"), PENDING_HEADER, &ps)?;
    persist_warnings(repo, s)?;
    // 4. Published checkpoints.
    let have = read_published(repo)?;
    let new: Vec<&Checkpoint> = s
        .published()
        .iter()
        .filter(|c| !have.contains(c) && !s.checkpoint_pending(c))
        .collect();
    append_published_all(repo, &new)?;
    let nck = new.len();
    Ok(Counts {
        ops: new_ops.len(),
        blobs: nblobs,
        checkpoints: nck,
    })
}

/// Find the `.hub/` directory for `start` (itself or an ancestor).
pub fn find_hub(start: &Path) -> Result<PathBuf, Error> {
    let mut cur = Some(start);
    while let Some(c) = cur {
        if c.join(HUB_DIR).is_dir() {
            return Ok(c.join(HUB_DIR));
        }
        cur = c.parent();
    }
    err(format!(
        "not a hub repository (no .hub/ in {} or any parent)",
        start.display()
    ))
}

fn dup_warning(peer: ReplicaId, other: &Path) -> RejectWarning {
    RejectWarning {
        peer,
        author: Some(peer),
        rejected: None,
        reason: RejectReason::DuplicateReplica,
        detail: format!(
            "repository {} uses our replica id; run `hub reid` in one copy",
            other.display()
        ),
        stuck: Vec::new(),
        first_seen: 0,
    }
}

/// `hub sync <path>`.
pub fn sync_cmd(
    cwd: &Path,
    peer_arg: &str,
    out: &mut dyn Write,
    warn: &mut dyn Write,
) -> Result<(), Error> {
    let stop = std::env::var(CRASH_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Interrupt::Abort);
    sync_with(cwd, peer_arg, stop, out, warn)
}

/// [`sync_cmd`] with an explicit test interruption point.
#[doc(hidden)]
pub fn sync_with(
    cwd: &Path,
    peer_arg: &str,
    stop: Option<Interrupt>,
    out: &mut dyn Write,
    warn: &mut dyn Write,
) -> Result<(), Error> {
    let mine = fs::canonicalize(find_hub(cwd)?)?;
    let peer_path = cwd.join(peer_arg);
    if !peer_path.is_dir() {
        return err(format!("no such directory: {peer_arg}"));
    }
    // Canonicalize before the upward walk: `a/../b` must not find a's hub.
    let theirs = fs::canonicalize(find_hub(&fs::canonicalize(&peer_path)?)?)?;
    if mine == theirs {
        return err("cannot sync a repository with itself");
    }
    // Lock order: canonical path, so opposite syncs cannot deadlock.
    let (mut a, mut b) = if mine < theirs {
        let a = Repo::open_dir_with(mine.clone(), LockMode::Exclusive)?;
        let b = Repo::open_dir_with(theirs.clone(), LockMode::Exclusive)?;
        (a, b)
    } else {
        let b = Repo::open_dir_with(theirs.clone(), LockMode::Exclusive)?;
        let a = Repo::open_dir_with(mine.clone(), LockMode::Exclusive)?;
        (a, b)
    };
    for w in a.warnings.iter().chain(&b.warnings) {
        writeln!(warn, "warning: {w}")?;
    }
    let (ra, rb) = (a.root(), b.root());
    let mut sa = load(&a)?;
    let mut sb = load(&b)?;
    let dup_msg = |why: &str| {
        Error(format!(
            "sync aborted: {why} (duplicate replica id, D-022). One repository is probably a \
             copy of the other's .hub/; run `hub reid` in one of them, then sync again"
        ))
    };
    if a.replica_id == b.replica_id {
        let wa = sa
            .warnings()
            .into_iter()
            .chain([dup_warning(b.replica_id, &rb)]);
        let wb = sb
            .warnings()
            .into_iter()
            .chain([dup_warning(a.replica_id, &ra)]);
        for (repo, ws) in [(&a, wa.collect::<Vec<_>>()), (&b, wb.collect())] {
            let mut ws2: Vec<RejectWarning> = Vec::new();
            for w in ws {
                if !ws2
                    .iter()
                    .any(|x| x.reason == w.reason && x.detail == w.detail)
                {
                    ws2.push(w);
                }
            }
            let bodies: Vec<String> = ws2.iter().map(encode_warning).collect();
            rewrite(&repo.dir.join("warnings.log"), WARNINGS_HEADER, &bodies)?;
        }
        return Err(dup_msg(&format!(
            "{} and {} have the same replica id {:032x}",
            ra.display(),
            rb.display(),
            a.replica_id.0
        )));
    }
    let dups = |s: &SyncReplica| {
        s.warnings()
            .iter()
            .filter(|w| w.reason == RejectReason::DuplicateReplica)
            .count()
    };
    let (da, db) = (dups(&sa), dups(&sb));
    core::sync(&mut sa, &mut sb, BATCH);
    if dups(&sa) > da || dups(&sb) > db {
        // Keep the warnings (D-021), persist no ops (D-022).
        persist_warnings(&a, &sa)?;
        persist_warnings(&b, &sb)?;
        return Err(dup_msg(
            "received ops authored under our own replica id that we never authored",
        ));
    }
    let mut appended = 0;
    let got = persist(&mut a, &sa, &mut appended, stop)?;
    let sent = persist(&mut b, &sb, &mut appended, stop)?;
    writeln!(
        out,
        "synced with {}: received {} op(s), {} blob(s), {} checkpoint(s); sent {} op(s), {} blob(s), {} checkpoint(s)",
        rb.display(),
        got.ops,
        got.blobs,
        got.checkpoints,
        sent.ops,
        sent.blobs,
        sent.checkpoints
    )?;
    let ix = a.index();
    let hid = VersionId::of_frontier(&a.head_frontier());
    let same = a.head_frontier() == b.head_frontier();
    writeln!(
        out,
        "head {}{}",
        ix.short(&hid),
        if same { " (both repositories)" } else { "" }
    )?;
    for (s, name) in [(&sa, "this repository"), (&sb, "peer")] {
        if s.pending_len() > 0 {
            writeln!(
                warn,
                "warning: {name}: {} op(s) pending (missing deps); retried on the next sync, see `hub warnings`",
                s.pending_len()
            )?;
        }
    }
    let nw = sa.warnings().len();
    if nw > 0 {
        writeln!(warn, "warning: {nw} sync warning(s); see `hub warnings`")?;
    }
    let idx = crate::wd::Index::load(&a.dir)?;
    if got.ops > 0 && idx.base != a.head_frontier() {
        writeln!(
            out,
            "note: the working directory is not updated by sync; run `hub checkout` to bring it to head"
        )?;
    }
    Ok(())
}

/// Ops in head but not in the down-closure of `base` (0 if `base` does not
/// resolve).
pub fn behind_by(repo: &Repo, base: &[hub_core::tree::OpId]) -> usize {
    match Version::downclose(&repo.store, base.iter().copied()) {
        Ok(v) => repo.store.len() - v.len(),
        Err(_) => 0,
    }
}

/// PROVISIONAL default (#23, pending user confirmation): after a successful
/// `hub reid`, drop this repository's saved `duplicate-replica` warnings,
/// since the id they complain about is no longer ours. Flip to `false` to
/// keep them until cleared by hand; `reid_clears_duplicate_replica_warnings`
/// in `tests/sync.rs` pins the current behavior.
pub const REID_CLEARS_DUPLICATE_WARNINGS: bool = true;

/// `hub reid`: a fresh CSPRNG replica id. Old ops keep their author id.
/// Run it before recording in a copied repository: ops already recorded
/// under the shared id still collide.
pub fn reid(repo: &mut Repo, out: &mut dyn Write) -> Result<(), Error> {
    let old = repo.replica_id;
    let mut new = core::new_replica_id();
    while new == old {
        new = core::new_replica_id();
    }
    store::write_atomic(
        &repo.dir.join("replica"),
        format!("{}\n{:032x}\n", store::REPLICA_HEADER, new.0).as_bytes(),
    )?;
    repo.replica_id = new;
    writeln!(out, "replica id {:032x} -> {:032x}", old.0, new.0)?;
    if REID_CLEARS_DUPLICATE_WARNINGS {
        let ws = read_warnings(&repo.dir)?;
        let keep: Vec<String> = ws
            .iter()
            .filter(|w| w.reason != RejectReason::DuplicateReplica)
            .map(encode_warning)
            .collect();
        let cleared = ws.len() - keep.len();
        if cleared > 0 {
            rewrite(&repo.dir.join("warnings.log"), WARNINGS_HEADER, &keep)?;
            writeln!(out, "cleared {cleared} duplicate-replica warning(s)")?;
        }
    }
    Ok(())
}
