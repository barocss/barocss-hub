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

use crate::Code;
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

/// Test-only instrumentation (Issue #34): when set, each sync appends
/// `blobs-in <this> <bytes> <count>` and `blobs-in <peer> <bytes> <count>`
/// lines (the blob bytes each side received) to this file.
#[doc(hidden)]
pub const STATS_LOG_ENV: &str = "HUB_TEST_SYNC_STATS";

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
    Error::new(
        Code::Corrupt,
        format!("corrupt {} line {}: {why}", path.display(), i + 2),
    )
}

fn hex_rid(t: &str) -> Result<ReplicaId, String> {
    if t.len() != 32 {
        return Err("bad replica id".into());
    }
    u128::from_str_radix(t, 16)
        .map(ReplicaId)
        .map_err(|_| "bad replica id".into())
}

/// One `pending.log` record: `P <peer 32 hex> <op record>` (a `TextEdit`
/// as its `T` record, Issue #54).
pub fn encode_pending(o: &Op, peer: PeerId) -> String {
    format!("P {:032x} {}", peer.0, store::encode_op(o))
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

pub fn reason_str(r: &RejectReason) -> &'static str {
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
    let check = repo.frontier_check();
    for (i, b) in read_opt(&p, PUBLISHED_HEADER)?.iter().enumerate() {
        let c = store::decode_ckpt(b, &check).map_err(|why| corrupt(&p, i, why))?;
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
///
/// Issue #32: no clone of the whole store. The replica only ever asks
/// whether a pending op or one of its deps is in the store, so a store of
/// exactly those stored ops answers every query identically (the only other
/// use of the store, the mint counter, is not read here).
pub fn warnings_view(repo: &Repo) -> Result<(Vec<RejectWarning>, usize), Error> {
    let pending = read_pending(&repo.dir)?;
    let store = if crate::oracle() {
        repo.store.clone()
    } else {
        pending_support(&repo.store, &pending)
    };
    let r = SyncReplica::from_parts(
        repo.replica_id,
        store,
        BlobStore::new(),
        pending,
        read_warnings(&repo.dir)?,
        Vec::new(),
    );
    Ok((r.warnings(), r.pending_len()))
}

/// The stored ops a set of pending ops can query: the pending ids and
/// their deps.
fn pending_support(
    store: &hub_core::version::Store,
    pending: &[(Op, PeerId)],
) -> hub_core::version::Store {
    pending
        .iter()
        .flat_map(|(o, _)| std::iter::once(o.id).chain(o.deps.iter().copied()))
        .filter_map(|id| store.get(&id).map(|o| (id, o.clone())))
        .collect()
}

fn short_rid(r: hub_core::tree::ReplicaId) -> String {
    format!("{:032x}", r.0)[..8].to_string()
}

/// The two sides of an equivocation (`kept`, `received`), from a detail
/// written by #45 or later (`None` for older, `Debug`-formatted details).
pub fn equivocation_sides(w: &RejectWarning) -> Option<(&str, &str)> {
    if w.reason != RejectReason::Equivocation {
        return None;
    }
    let (k, r) = w.detail.split_once('\n')?;
    Some((k.strip_prefix("kept: ")?, r.strip_prefix("received: ")?))
}

/// What the user should do about a warning (Issue #45).
pub fn warning_action(w: &RejectWarning) -> &'static str {
    match w.reason {
        RejectReason::Equivocation => {
            "two repositories share a replica id (a copied .hub/): run `barocss-hub reid --rescue` in the copy (it re-authors the dropped change under a fresh id)"
        }
        RejectReason::LamportViolation => {
            "the op was not made by a correct barocss-hub; ask the author to upgrade, and redo the change"
        }
        RejectReason::CorruptBlob => "sync again; the blob is fetched again from a peer with a good copy",
        RejectReason::DuplicateReplica => "run `barocss-hub reid --rescue` in the copied repository",
    }
}

/// One warning as a human sentence (who, which op, what differs, what to
/// do). Issue #45: no Rust `Debug` text.
pub fn render_warning(w: &RejectWarning) -> String {
    let peer = short_rid(w.peer);
    let author = w.author.map(short_rid);
    let op = w.rejected.map(store::fmt_id);
    let mut s = match (&w.reason, equivocation_sides(w)) {
        (RejectReason::Equivocation, Some((k, r))) => format!(
            "equivocation: replica {} used op id {} for two different changes (received from peer {peer}); this repository kept: {k}; it dropped: {r}",
            author.as_deref().unwrap_or("?"),
            op.as_deref().unwrap_or("?"),
        ),
        (RejectReason::LamportViolation, _) => format!(
            "lamport-violation: op {} by replica {} (received from peer {peer}) was rejected: {}",
            op.as_deref().unwrap_or("?"),
            author.as_deref().unwrap_or("?"),
            w.detail
        ),
        _ => {
            let mut s = format!("{} from peer {peer}", reason_str(&w.reason));
            if let Some(r) = &op {
                s.push_str(&format!(" op {r}"));
            }
            s.push_str(&format!(": {}", w.detail.replace('\n', "; ")));
            s
        }
    };
    if !w.stuck.is_empty() {
        let v: Vec<String> = w.stuck.iter().map(|o| store::fmt_id(*o)).collect();
        s.push_str(&format!("; stuck: {}", v.join(", ")));
    }
    s.push_str(&format!(". What to do: {}", warning_action(w)));
    s
}

fn content_blob(o: &Op) -> Option<hub_core::tree::BlobId> {
    match &o.payload {
        Payload::SetContent { blob, .. } => Some(*blob),
        _ => None,
    }
}

/// Load a side for sync. Issue #34: blob bytes are not read. Blobs named by
/// applied ops are only checked to exist (one directory listing; the
/// applied-set invariant says they were durable before their op) and marked
/// held; they are read, verified, only if they are sent
/// ([`fetch_blob`]). Blobs of pending ops are read and verified as before.
pub(crate) fn load(repo: &Repo) -> Result<SyncReplica, Error> {
    load_opts(repo, true)
}

/// [`load`]; `write = false` (a read-only `serve`) never creates, sweeps or
/// otherwise touches a file.
pub(crate) fn load_opts(repo: &Repo, write: bool) -> Result<SyncReplica, Error> {
    let bdir = repo.dir.join("blobs");
    let on_disk: std::collections::HashSet<_> = if write {
        let disk = repo.blobs()?;
        disk.sweep_temps()?;
        crate::session::sweep_wire_files(&bdir);
        disk.list()?.into_iter().collect()
    } else if bdir.is_dir() {
        crate::blobs::list_dir(&bdir)?.into_iter().collect()
    } else {
        Default::default()
    };
    let mut bs = BlobStore::new();
    for o in repo.store.values() {
        if let Some(b) = content_blob(o) {
            if b != hub_core::tree::BlobId::empty() && !on_disk.contains(&b) {
                return Err(Error::new(
                    Code::NoSuchBlob,
                    format!(
                        "cannot sync {}: {} (named by op {})",
                        repo.root().display(),
                        crate::blobs::BlobError::Missing(b),
                        store::fmt_id(o.id)
                    ),
                ));
            }
            bs.mark_held(b);
        }
    }
    let pending = read_pending(&repo.dir)?;
    for (o, _) in &pending {
        if let Some(b) = content_blob(o) {
            if let Ok(bytes) = crate::blobs::read_verified(&bdir, &b) {
                bs.put(&bytes);
            }
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub ops: usize,
    pub blobs: usize,
    pub checkpoints: usize,
}

pub(crate) fn persist_warnings(repo: &Repo, s: &SyncReplica) -> Result<(), Error> {
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

/// Persist a merged replica (A.1 order), where some received blobs are verified files (`files`,
/// Issue #43): they are renamed into place, never re-read.
pub(crate) fn persist_with(
    repo: &mut Repo,
    s: &SyncReplica,
    files: &std::collections::HashMap<hub_core::tree::BlobId, PathBuf>,
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
    // Issue #54: format v2 before a received TextEdit reaches ops.log or
    // pending.log.
    if new_ops
        .iter()
        .chain(pending.iter().map(|(o, _)| o))
        .any(|o| matches!(o.payload, hub_core::tree::Payload::TextEdit(_)))
    {
        repo.ensure_text_format()?;
    }
    // 1. Blobs first: an op on disk never names a missing blob.
    // Issue #34: only loaded bytes can be new (received, or a pending op's
    // blob read from this disk, which `add` skips); held blobs are on disk.
    let disk = repo.blobs()?;
    let want: BTreeSet<_> = new_ops
        .iter()
        .chain(pending.iter().map(|(o, _)| o))
        .filter_map(content_blob)
        .collect();
    // Group commit (Issue #29): one barrier for all; direct writes, no
    // renames (Issue #34, Appendix A.1).
    let mut batch = disk.batch_direct();
    for b in want {
        if let Some(bytes) = s.blobs().get(&b) {
            batch.add(bytes).map_err(crate::blob_error)?;
        } else if let Some(f) = files.get(&b) {
            batch.add_file(&b, f).map_err(crate::blob_error)?;
        }
    }
    let nblobs = batch.commit().map_err(crate::blob_error)?;
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
    let ps: Vec<String> = pending.iter().map(|(o, p)| encode_pending(o, *p)).collect();
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
    crate::errc(
        Code::NotARepository,
        format!(
            "not a hub repository (no .hub/ in {} or any parent)",
            start.display()
        ),
    )
}

/// `hub sync <path>` (local `file://`, Issue #22; over the wire protocol
/// since #43, see [`sync_local`]).
pub fn sync_cmd(
    cwd: &Path,
    peer_arg: &str,
    out: &mut dyn Write,
    warn: &mut dyn Write,
) -> Result<(), Error> {
    crate::remote::sync_cmd(cwd, Some(peer_arg), false, out, warn)
}

/// The test crash hook from the environment ([`CRASH_ENV`]).
pub(crate) fn env_stop() -> Option<Interrupt> {
    hook_var!(CRASH_ENV)
        .and_then(|v| v.parse().ok())
        .map(Interrupt::Abort)
}

/// Local sync with an explicit test interruption point.
#[doc(hidden)]
pub fn sync_with(
    cwd: &Path,
    peer_arg: &str,
    stop: Option<Interrupt>,
    out: &mut dyn Write,
    warn: &mut dyn Write,
) -> Result<(), Error> {
    let peer_path = cwd.join(peer_arg);
    let r = sync_local(cwd, &peer_path, stop, warn)?;
    print_report(&r, out, warn)?;
    r.remember(warn)
}

/// A finished sync, for printing and for `watch`/`--json`.
pub struct Synced {
    pub remote: String,
    pub report: crate::session::Report,
    pub peer_pending: Option<usize>,
    pub pending: usize,
    pub warnings: usize,
    pub head_short: String,
    pub head_id: String,
    pub behind_note: bool,
    /// `report.rejected` classified against the reported-rejections
    /// memory (Issue #50, D-050).
    pub rej: Vec<crate::reported::Rejection>,
    /// The remote's trust key (the memory key) and our `.hub/`.
    pub key: Option<crate::trust::TrustKey>,
    pub hub: std::path::PathBuf,
}

impl Synced {
    /// Rejections not reported before (these warn and exit 3).
    pub fn new_rejected(&self) -> Vec<(hub_core::tree::OpId, String)> {
        self.rej
            .iter()
            .filter(|r| !r.already)
            .map(|r| (r.op, r.reason.clone()))
            .collect()
    }

    /// Record the new rejections as reported; call after printing them.
    /// A failure is a warning, not a sync failure (the next sync reports
    /// again).
    pub fn remember(&self, warn: &mut dyn Write) -> Result<(), Error> {
        if let Err(e) = crate::reported::remember(&self.hub, self.key.as_ref(), &self.rej) {
            writeln!(warn, "warning: could not remember reported rejections: {e}")?;
        }
        Ok(())
    }
}

/// What a rejection reason means for the author, and what to do.
pub fn rejected_message(reason: &str) -> &'static str {
    match reason {
        "equivocation" => "the remote already has a different op with this id (another repository shares this replica id, probably a copied .hub/); run `barocss-hub reid --rescue` here (it re-authors the change under a fresh id)",
        "lamport-violation" => "its deps are not older than the op (not made by a correct barocss-hub); redo the change with an up-to-date barocss-hub",
        _ => "rejected by the remote",
    }
}

/// One human line per op the remote rejected (Issue #45).
pub fn rejected_lines(rej: &[(hub_core::tree::OpId, String)]) -> Vec<String> {
    rej.iter()
        .map(|(id, why)| {
            format!(
                "the remote rejected your op {} ({why}): {}",
                store::fmt_id(*id),
                rejected_message(why)
            )
        })
        .collect()
}

/// Print a [`Synced`] the way `hub sync` always has.
pub fn print_report(r: &Synced, out: &mut dyn Write, warn: &mut dyn Write) -> Result<(), Error> {
    let (got, sent) = (r.report.received, r.report.sent);
    writeln!(
        out,
        "synced with {}: received {} op(s), {} blob(s), {} checkpoint(s); sent {} op(s), {} blob(s), {} checkpoint(s)",
        r.remote, got.ops, got.blobs, got.checkpoints, sent.ops, sent.blobs, sent.checkpoints
    )?;
    if r.report.read_only {
        writeln!(out, "remote is read-only; pulled only")?;
    }
    writeln!(
        out,
        "head {}{}",
        r.head_short,
        if r.report.same_head {
            " (both repositories)"
        } else {
            ""
        }
    )?;
    for (n, name) in [
        (Some(r.pending), "this repository"),
        (r.peer_pending, "peer"),
    ] {
        if let Some(n) = n.filter(|n| *n > 0) {
            writeln!(
                warn,
                "warning: {name}: {n} op(s) pending (missing deps); retried on the next sync, see `barocss-hub warnings`"
            )?;
        }
    }
    let new = r.new_rejected();
    if !new.is_empty() {
        crate::note_rejected();
    }
    for l in rejected_lines(&new) {
        writeln!(warn, "warning: {l}")?;
    }
    if r.warnings > 0 {
        writeln!(
            warn,
            "warning: {} sync warning(s); see `barocss-hub warnings`",
            r.warnings
        )?;
    }
    if r.behind_note {
        writeln!(
            out,
            "note: the working directory is not updated by sync; run `barocss-hub checkout` to bring it to head"
        )?;
    }
    Ok(())
}

/// Build the [`Synced`] summary from the client side after a session.
pub(crate) fn summarize(
    remote: String,
    side: &crate::session::Side,
    report: crate::session::Report,
    peer_pending: Option<usize>,
    key: Option<crate::trust::TrustKey>,
) -> Result<Synced, Error> {
    let repo = side.repo().expect("client side holds its repo");
    let s = side.replica();
    let ix = repo.index();
    let hid = VersionId::of_frontier(&repo.head_frontier());
    let idx = crate::wd::Index::load(&repo.dir)?;
    let rej = crate::reported::classify(repo, key.as_ref(), &report.rejected);
    Ok(Synced {
        rej,
        key,
        hub: repo.dir.clone(),
        remote,
        behind_note: report.received.ops > 0 && idx.base != repo.head_frontier(),
        report,
        peer_pending,
        pending: s.pending_len(),
        warnings: s.warnings().len(),
        head_short: ix.short(&hid),
        head_id: hid.hex(),
    })
}

/// Sync with a local repository over the wire protocol (`file://`, §5.3.3)
/// through an in-process pipe, the far end on a thread. Both repositories
/// are locked for the whole session in canonical-path order (as #22), so
/// two opposite local syncs cannot deadlock; see the #43 report for why
/// the served end does not use the per-step lock here.
pub fn sync_local(
    cwd: &Path,
    peer_path: &Path,
    stop: Option<Interrupt>,
    warn: &mut dyn Write,
) -> Result<Synced, Error> {
    use crate::session::{self, Access, Side};
    let mine = fs::canonicalize(find_hub(cwd)?)?;
    if !peer_path.is_dir() {
        return crate::errc(
            Code::NoSuchPath,
            format!("no such directory: {}", peer_path.display()),
        );
    }
    // Canonicalize before the upward walk: `a/../b` must not find a's hub.
    let theirs = fs::canonicalize(find_hub(&fs::canonicalize(peer_path)?)?)?;
    if mine == theirs {
        return crate::errc(Code::Usage, "cannot sync a repository with itself");
    }
    let (a, b) = if mine < theirs {
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
    let rb = b.root();
    let mut ca = Side::new(Access::Held(Box::new(a)), false)?;
    let mut cb = Side::new(Access::Held(Box::new(b)), false)?;
    ca.stop = stop;
    cb.stop = stop;
    cb.appended = ca.appended.clone();
    let (mut x, mut y, _cut) = crate::wire::pair();
    let th = std::thread::spawn(move || {
        let r = session::server(&mut y, &mut cb);
        (r, cb)
    });
    let rc = session::client(&mut x, &mut ca);
    drop(x);
    let (rs, cb) = th
        .join()
        .map_err(|_| Error::new(Code::Error, "sync thread panicked"))?;
    let interrupted = |r: &Result<(), Error>| matches!(r, Err(e) if e.0 == INTERRUPTED);
    if interrupted(&rs) {
        return Err(rs.unwrap_err());
    }
    // Review R1 (#46): when the far end stopped first with a real error
    // (e.g. `duplicate-replica`), the near end may only have seen the
    // closed pipe; the far end's error is the cause.
    let report = match (rc, rs) {
        (Err(e), Err(s)) if e.code() == Code::Io && s.code() != Code::Io => return Err(s),
        (rc, _) => rc?,
    };
    if let Some(p) = hook_os!(STATS_LOG_ENV) {
        let mut f = fs::OpenOptions::new().create(true).append(true).open(p)?;
        for (name, s) in [("this", &ca), ("peer", &cb)] {
            writeln!(f, "blobs-in {name} {} {}", s.blob_bytes_in, s.blobs_in)?;
        }
    }
    let peer_pending = Some(session::pending_len(&cb));
    let key = crate::remote::file_key(&theirs, &mine);
    summarize(
        rb.display().to_string(),
        &ca,
        report,
        peer_pending,
        Some(key),
    )
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

/// Drop the saved warnings matching `drop` (Issue #49). Returns how many.
pub(crate) fn drop_warnings(
    dir: &Path,
    drop: impl Fn(&RejectWarning) -> bool,
) -> Result<usize, Error> {
    let ws = read_warnings(dir)?;
    let keep: Vec<String> = ws.iter().filter(|w| !drop(w)).map(encode_warning).collect();
    let n = ws.len() - keep.len();
    if n > 0 {
        rewrite(&dir.join("warnings.log"), WARNINGS_HEADER, &keep)?;
    }
    Ok(n)
}

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
        format!(
            "{}\n{:032x}\n",
            store::replica_header(repo.is_v2(), false),
            new.0
        )
        .as_bytes(),
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

/// Issue #32: `warnings_view` over the pending-support sub-store equals the
/// pre-#32 whole-store clone (warnings with recomputed `stuck`, pending
/// count) for random pending buffers: deps present, missing, on other
/// pending ops, not `<` the op (Lamport violations drained into warnings),
/// and SetContent of present and absent blobs, with saved warnings of every
/// reason.
#[cfg(test)]
mod warnings_view_tests {
    use super::*;
    use hub_core::tree::{Kind, NodeId, ROOT};
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: crate::test_cases(128), ..ProptestConfig::default() })]

        #[test]
        fn warnings_view_equals_full_clone(seed in any::<u64>(), nops in 1usize..12, npend in 0usize..10) {
            let mut s = seed | 1;
            let mut rnd = move |n: usize| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s % n.max(1) as u64) as usize
            };
            let root = std::env::temp_dir().join(format!("hub-wv32-{}-{seed:x}-{nops}-{npend}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let mut repo = Repo::init(&root).unwrap();
            let mut ids = Vec::new();
            for i in 0..nops {
                let deps: BTreeSet<_> = (0..rnd(3)).filter(|_| !ids.is_empty()).map(|_| ids[rnd(ids.len())]).collect();
                let id = NodeId::new(i as u64 + 2, 1 + rnd(2) as u128);
                repo.append_op(Op { id, deps, payload: Payload::Create { kind: Kind::File, parent: ROOT, name: format!("f{i}") } }).unwrap();
                ids.push(id);
            }
            let mut pend: Vec<Op> = Vec::new();
            let blob = hub_core::tree::BlobId::of(b"absent");
            for k in 0..npend {
                let id = NodeId::new(100 + k as u64, 7);
                let mut deps = BTreeSet::new();
                match rnd(5) {
                    0 => { deps.insert(ids[rnd(ids.len())]); }
                    1 => { deps.insert(NodeId::new(50, 9)); }
                    2 if !pend.is_empty() => { deps.insert(pend[rnd(pend.len())].id); }
                    3 => { deps.insert(NodeId::new(200 + rnd(5) as u64, 7)); }
                    _ => {}
                }
                let payload = if rnd(3) == 0 {
                    Payload::SetContent { node: ids[rnd(ids.len())], blob: if rnd(2) == 0 { blob } else { hub_core::tree::BlobId::empty() } }
                } else {
                    Payload::Create { kind: Kind::Directory, parent: ROOT, name: format!("p{k}") }
                };
                pend.push(Op { id, deps, payload });
            }
            let peer = ReplicaId(0xbeef);
            let ps: Vec<String> = pend.iter().map(|o| encode_pending(o, peer)).collect();
            rewrite(&repo.dir.join("pending.log"), PENDING_HEADER, &ps).unwrap();
            let mut ws = Vec::new();
            for (k, reason) in ["lamport-violation", "corrupt-blob", "equivocation"].iter().enumerate() {
                if rnd(2) == 0 {
                    let rej = if pend.is_empty() { "-".to_string() } else { store::fmt_id(pend[rnd(pend.len())].id) };
                    let detail = if *reason == "corrupt-blob" { blob.hex() } else { format!("d{k}") };
                    ws.push(format!("W {:032x} {:032x} {rej} {reason} {k} {}", peer.0, 7u128, store::esc(&detail)));
                }
            }
            rewrite(&repo.dir.join("warnings.log"), WARNINGS_HEADER, &ws).unwrap();
            let render = |v: (Vec<RejectWarning>, usize)| (v.0.iter().map(render_warning).collect::<Vec<_>>(), v.1);
            let new = render(warnings_view(&repo).unwrap());
            crate::ORACLE.with(|o| o.set(true));
            let old = render(warnings_view(&repo).unwrap());
            crate::ORACLE.with(|o| o.set(false));
            drop(repo);
            let _ = std::fs::remove_dir_all(&root);
            prop_assert_eq!(new, old);
        }
    }
}
