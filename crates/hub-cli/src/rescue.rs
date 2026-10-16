//! `barocss-hub reid --rescue` (Issue #49, D-049; Wiki Repository-Semantics
//! 4.13).
//!
//! A copied repository whose ops under a shared replica id diverged from
//! (or were rejected by) a remote re-authors them **by effect**: the
//! rescue set `R` (ops named by our saved equivocation warnings or by the
//! reported-rejections memory, plus every local descendant) is removed
//! from the local log, the repository takes a fresh replica id, the index
//! is rebased onto the new head, and the working directory (including
//! unrecorded edits) is recorded as new ops under the new id, with Lamport
//! counters above every removed op.
//!
//! Crash safety (the #33 bar): `.hub/rescue/<ts>/` holds the `manifest`
//! and a backup of every differing working-tree file (`files/`);
//! `.hub/rescue/current` marks a rescue in progress (mutating commands
//! refuse while it exists). Stages 1-6 are idempotent and recomputed from
//! disk on a rerun, each followed by a crash point.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use hub_core::projection::project;
use hub_core::sync::{self as core, RejectReason};
use hub_core::tree::{evaluate_with, Op, OpId, ReplicaId};

use crate::json::{self, J};
use crate::store::{self, LockMode, RepoLock};
use crate::wd::{self, Change, Target};
use crate::{durable, errc, remote, reported, sync, watch, Code, Error, Repo};

pub const DIR: &str = "rescue";
pub const CURRENT: &str = "current";
pub const MANIFEST: &str = "manifest";
pub const MANIFEST_HEADER: &str = "barocss-hub/rescue/v0";
pub const DONE: &str = "done";
pub const FILES: &str = "files";

/// Crash points, in stage order (tests).
pub const C_BACKUP: &str = "rescue-backup";
pub const C_MANIFEST: &str = "rescue-manifest";
pub const C_CKPTS: &str = "rescue-ckpts";
pub const C_SNAPSHOT: &str = "rescue-snapshot";
pub const C_OPS_TMP: &str = "rescue-ops-tmp";
pub const C_OPS: &str = "rescue-ops";
pub const C_REID: &str = "rescue-reid";
pub const C_INDEX: &str = "rescue-index";
pub const C_RECORD: &str = "rescue-record";
pub const C_WARNINGS: &str = "rescue-warnings";
pub const C_DONE: &str = "rescue-done";
pub const C_MARKED: &str = "rescue-marked";
pub const C_CKPTS_RESTORED: &str = "rescue-ckpts-restored";
pub const C_UNMARK: &str = "rescue-unmark";
pub const STAGES: &[&str] = &[
    C_BACKUP,
    C_MANIFEST,
    C_MARKED,
    C_CKPTS,
    C_SNAPSHOT,
    C_OPS_TMP,
    C_OPS,
    C_REID,
    C_INDEX,
    C_RECORD,
    C_CKPTS_RESTORED,
    C_WARNINGS,
    C_DONE,
    C_UNMARK,
];

#[derive(Clone, Copy, Debug, Default)]
pub struct Opts {
    pub dry_run: bool,
    pub offline: bool,
    pub json: bool,
}

/// What a rescue does (persisted as the manifest).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Manifest {
    old: ReplicaId,
    new: ReplicaId,
    floor: u64,
    ops: Vec<Op>,
    /// (change, path): working-tree files that differ from store minus R.
    files: Vec<(String, String)>,
    /// Checkpoint records removed in stage 1 (R1 repair): (published, raw
    /// `K` body), re-created as private checkpoints on the rescued head.
    ckpts: Vec<(bool, String)>,
}

/// Whether a rescue is in progress in `.hub/` `hub`.
pub fn in_progress(hub: &Path) -> bool {
    hub.join(DIR).join(CURRENT).exists()
}

/// The refusal of a mutating command while a rescue is in progress.
pub fn refuse_if_in_progress(hub: &Path, cmd: &str) -> Result<(), Error> {
    if in_progress(hub) {
        return errc(
            Code::Blocked,
            format!(
                "refusing to {cmd}: a `barocss-hub reid --rescue` was interrupted; run it again to finish (the backup is in {})",
                hub.join(DIR).display()
            ),
        );
    }
    Ok(())
}

fn rid_hex(r: ReplicaId) -> String {
    format!("{:032x}", r.0)
}

fn dsync(dir: &Path) -> Result<(), Error> {
    let d = File::open(dir)?;
    durable::flush_dir(dir, &d)?;
    durable::barrier(&d)?;
    Ok(())
}

/// temp + flush + barrier + rename + directory sync.
fn write_durable(path: &Path, text: &str, crash: Option<&str>) -> Result<(), Error> {
    let dir = path.parent().expect("path has a parent");
    let tmp = dir.join(format!(
        ".{}.rescue-tmp",
        path.file_name().unwrap().to_string_lossy()
    ));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        durable::flush(&f)?;
        durable::barrier(&f)?;
    }
    if let Some(c) = crash {
        durable::crash_point(c);
    }
    fs::rename(&tmp, path)?;
    dsync(dir)
}

fn encode_manifest(m: &Manifest) -> String {
    let mut s = format!("{MANIFEST_HEADER}\n");
    s.push_str(&store::frame(&format!("old {}", rid_hex(m.old))));
    s.push_str(&store::frame(&format!("new {}", rid_hex(m.new))));
    s.push_str(&store::frame(&format!("floor {}", m.floor)));
    for o in &m.ops {
        s.push_str(&store::frame(&format!("op {}", store::encode_op(o))));
    }
    for (c, p) in &m.files {
        s.push_str(&store::frame(&format!("file {c} {}", store::esc(p))));
    }
    for (publ, b) in &m.ckpts {
        let k = if *publ { "published" } else { "private" };
        s.push_str(&store::frame(&format!("ckpt {k} {b}")));
    }
    s
}

fn parse_rid(t: &str) -> Result<ReplicaId, String> {
    if t.len() != 32 {
        return Err("bad replica id".into());
    }
    u128::from_str_radix(t, 16)
        .map(ReplicaId)
        .map_err(|_| "bad replica id".into())
}

fn read_manifest(path: &Path) -> Result<Manifest, Error> {
    let l = store::read_log_with(path, MANIFEST_HEADER, false)?;
    let bad = |why: String| Error::new(Code::Corrupt, format!("corrupt {}: {why}", path.display()));
    let (mut old, mut new, mut floor) = (None, None, None);
    let mut ops = Vec::new();
    let mut files = Vec::new();
    let mut ckpts = Vec::new();
    for b in &l.records {
        let (k, v) = b.split_once(' ').ok_or_else(|| bad("bad record".into()))?;
        match k {
            "old" => old = Some(parse_rid(v).map_err(bad)?),
            "new" => new = Some(parse_rid(v).map_err(bad)?),
            "floor" => floor = Some(v.parse().map_err(|_| bad("bad floor".into()))?),
            "op" => ops.push(store::decode_op(v).map_err(bad)?),
            "file" => {
                let (c, p) = v.split_once(' ').ok_or_else(|| bad("bad file".into()))?;
                files.push((c.to_string(), store::unesc(p).map_err(bad)?));
            }
            "ckpt" => {
                let (k, body) = v.split_once(' ').ok_or_else(|| bad("bad ckpt".into()))?;
                store::split_ckpt(body).map_err(bad)?;
                ckpts.push((k == "published", body.to_string()));
            }
            _ => return Err(bad(format!("unknown record '{k}'"))),
        }
    }
    match (old, new, floor) {
        (Some(old), Some(new), Some(floor)) => Ok(Manifest {
            old,
            new,
            floor,
            ops,
            files,
            ckpts,
        }),
        _ => Err(bad("incomplete manifest".into())),
    }
}

/// The rescue set: seeds from saved equivocation warnings authored under
/// our current id and from the reported-rejections memory, up-closed in
/// the local store (Wiki 4.13 step 1).
fn rescue_set(repo: &Repo) -> Result<BTreeSet<OpId>, Error> {
    let mut seeds: BTreeSet<OpId> = BTreeSet::new();
    for w in sync::read_warnings(&repo.dir)? {
        if w.reason == RejectReason::Equivocation && w.author == Some(repo.replica_id) {
            if let Some(id) = w.rejected {
                seeds.insert(id);
            }
        }
    }
    for e in reported::read(&repo.dir) {
        seeds.insert(e.op);
    }
    seeds.retain(|id| repo.store.contains_key(id));
    let mut r = seeds.clone();
    // The store iterates in `<` order and a dep is `<` its op.
    for (id, o) in repo.store.iter() {
        if !r.contains(id) && o.deps.iter().any(|d| r.contains(d)) {
            r.insert(*id);
        }
    }
    Ok(r)
}

fn has_dup_block(repo: &Repo) -> Result<bool, Error> {
    Ok(sync::read_warnings(&repo.dir)?
        .iter()
        .any(|w| w.reason == RejectReason::DuplicateReplica))
}

fn change_rows(changes: &[Change]) -> Vec<(String, String)> {
    let mut v = Vec::new();
    for c in changes {
        match c {
            Change::Added(p) => v.push(("added".into(), p.clone())),
            Change::Modified(p) => v.push(("modified".into(), p.clone())),
            Change::Deleted(p) => v.push(("deleted".into(), p.clone())),
            Change::Renamed(a, b) => {
                v.push(("deleted".into(), a.clone()));
                v.push(("added".into(), b.clone()));
            }
        }
    }
    v
}

/// The working-tree files that differ from the store without `r`.
fn differing(repo: &Repo, r: &BTreeSet<OpId>) -> Result<Vec<(String, String)>, Error> {
    let kept: Vec<&Op> = repo.store.values().filter(|o| !r.contains(&o.id)).collect();
    let deps: BTreeSet<OpId> = kept.iter().flat_map(|o| o.deps.iter().copied()).collect();
    let frontier: Vec<OpId> = kept
        .iter()
        .map(|o| o.id)
        .filter(|i| !deps.contains(i))
        .collect();
    // Issue #56: text heads derived with the seeds (never `NoBlobs`).
    let state = evaluate_with(kept.iter().copied(), &repo.disk_blobs()).state;
    let proj = project(&state);
    let idx = wd::index_for(
        &Target {
            state: &state,
            proj: &proj,
            frontier,
        },
        wd::case_insensitive(&repo.dir)?,
    );
    let mut ws = Vec::new();
    let w = wd::scan(&repo.root(), &idx, &mut ws)?;
    Ok(change_rows(&wd::diff(&idx, &w)))
}

fn check_at_head(repo: &Repo) -> Result<(), Error> {
    if wd::interrupted(&repo.dir)?.is_some() {
        return errc(
            Code::Interrupted,
            "a checkout was interrupted; run `barocss-hub checkout` to finish it before `reid --rescue`",
        );
    }
    let idx = wd::Index::load(&repo.dir)?;
    if idx.base != repo.head_frontier() && !(idx.base.is_empty() && repo.store.is_empty()) {
        return errc(
            Code::Behind,
            "the working directory is not at head; run `barocss-hub checkout` first, then `barocss-hub reid --rescue` (rescue records the working directory as you see it)",
        );
    }
    Ok(())
}

fn now_ts() -> String {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n:024}")
}

fn current_ts(hub: &Path) -> Result<Option<String>, Error> {
    match fs::read_to_string(hub.join(DIR).join(CURRENT)) {
        Ok(t) => Ok(Some(t.trim().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Remove rescue directories of runs that crashed before `current` was
/// written (no `done`, not current).
fn sweep_orphans(hub: &Path) {
    let Ok(rd) = fs::read_dir(hub.join(DIR)) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() && !p.join(DONE).exists() {
            let _ = fs::remove_dir_all(&p);
        }
    }
}

/// Copy the differing files into `files/`, each flushed.
fn backup(root: &Path, dir: &Path, files: &[(String, String)]) -> Result<usize, Error> {
    let fdir = dir.join(FILES);
    let mut n = 0;
    for (c, p) in files {
        if c != "added" && c != "modified" {
            continue;
        }
        let src = root.join(p);
        if !fs::symlink_metadata(&src).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let dst = fdir.join(p);
        fs::create_dir_all(dst.parent().unwrap())?;
        fs::copy(&src, &dst)?;
        let f = File::open(&dst)?;
        durable::flush(&f)?;
        n += 1;
    }
    if fdir.exists() {
        dsync(&fdir)?;
    }
    Ok(n)
}

fn filter_ckpt_log(path: &Path, header: &str, r: &BTreeSet<OpId>) -> Result<usize, Error> {
    if !path.exists() {
        return Ok(0);
    }
    let l = store::read_log(path, header)?;
    let keep: Vec<&String> = l
        .records
        .iter()
        .filter(|b| match store::split_ckpt(b) {
            Ok((ids, ..)) => !ids.iter().any(|i| r.contains(i)),
            Err(_) => true,
        })
        .collect();
    let n = l.records.len() - keep.len();
    if n > 0 {
        let mut s = format!("{header}\n");
        for b in keep {
            s.push_str(&store::frame(b));
        }
        write_durable(path, &s, None)?;
    }
    Ok(n)
}

/// The checkpoint records of `path` that name an op of `r`.
fn ckpts_naming(path: &Path, header: &str, r: &BTreeSet<OpId>) -> Result<Vec<String>, Error> {
    if !path.exists() || r.is_empty() {
        return Ok(Vec::new());
    }
    let l = store::read_log(path, header)?;
    Ok(l.records
        .into_iter()
        .filter(
            |b| matches!(store::split_ckpt(b), Ok((ids, ..)) if ids.iter().any(|i| r.contains(i))),
        )
        .collect())
}

/// The replica header while a rescue is in progress (R3 repair, the #33
/// technique): pre-#49 binaries refuse it ("unsupported repository
/// format") before reading or writing anything else.
fn set_replica(hub: &Path, rid: ReplicaId, rescuing: bool) -> Result<(), Error> {
    // Issue #54: the marker keeps the format (v1-rescue / v2-rescue).
    let v2 = matches!(
        crate::migrate::read_replica(hub),
        Ok((crate::migrate::Format::V2, _))
    );
    let h = store::replica_header(v2, rescuing);
    let want = format!("{h}\n{}\n", rid_hex(rid));
    if fs::read_to_string(hub.join("replica")).ok().as_deref() != Some(want.as_str()) {
        write_durable(&hub.join("replica"), &want, None)?;
    }
    Ok(())
}

/// Whether the replica header carries the rescue marker.
pub fn marked(hub: &Path) -> bool {
    fs::read_to_string(hub.join("replica")).is_ok_and(|t| {
        matches!(
            t.lines().next(),
            Some(store::REPLICA_HEADER_RESCUE | store::REPLICA_HEADER_V2_RESCUE)
        )
    })
}

/// Stages 1-4 on the raw files, under the exclusive lock.
fn rewrite_stages(hub: &Path, m: &Manifest) -> Result<(), Error> {
    let _lock = RepoLock::acquire(hub, LockMode::Exclusive)
        .map_err(|e| Error::new(Code::Locked, format!("cannot lock {}: {e}", hub.display())))?;
    let r: BTreeSet<OpId> = m.ops.iter().map(|o| o.id).collect();
    // 1. Checkpoints naming R.
    if !r.is_empty() {
        filter_ckpt_log(&hub.join("checkpoints.log"), store::CKPT_HEADER, &r)?;
        filter_ckpt_log(&hub.join("published.log"), sync::PUBLISHED_HEADER, &r)?;
    }
    durable::crash_point(C_CKPTS);
    // 2. The snapshot cache (it names the old log).
    if !r.is_empty() && hub.join(crate::snapshot::FILE).exists() {
        fs::remove_file(hub.join(crate::snapshot::FILE))?;
        dsync(hub)?;
    }
    durable::crash_point(C_SNAPSHOT);
    // 3. ops.log without R, one batch, in the original order.
    let op = hub.join("ops.log");
    let all = store::read_oplog(&op, None)?;
    let mut keep: Vec<&String> = Vec::new();
    let mut dropped = 0;
    for b in &all.records {
        match store::decode_op(b) {
            Ok(o) if r.contains(&o.id) => dropped += 1,
            _ => keep.push(b),
        }
    }
    if dropped > 0 {
        let mut text = format!("{}\n", store::OPLOG_HEADER);
        let mut crcs = Vec::with_capacity(keep.len());
        for b in &keep {
            let line = store::frame(b);
            crcs.push(line[line.len() - 9..line.len() - 1].to_string());
            text.push_str(&line);
        }
        if !crcs.is_empty() {
            let chain = store::batch_chain(store::CHAIN0, crcs.iter().map(String::as_str));
            text.push_str(&store::frame(&store::commit_body(
                1,
                crcs.len() as u64,
                &chain,
            )));
        }
        write_durable(&op, &text, Some(C_OPS_TMP))?;
    }
    durable::crash_point(C_OPS);
    // 4. The fresh replica id.
    set_replica(hub, m.new, true)?;
    durable::crash_point(C_REID);
    Ok(())
}

struct Outcome {
    recorded: Vec<crate::ChangeInfo>,
    /// Re-created checkpoints: (message, was published, short id).
    restored: Vec<(String, bool, String)>,
    record_lines: Vec<String>,
}

/// Stages 5-6 on the reopened repository.
fn record_stages(
    hub: &Path,
    dir: &Path,
    m: &Manifest,
    warn: &mut dyn Write,
) -> Result<Outcome, Error> {
    let mut repo = Repo::open_dir_with(hub.to_path_buf(), LockMode::Exclusive)?;
    for w in &repo.warnings {
        writeln!(warn, "warning: {w}")?;
    }
    repo.counter_floor = m.floor;
    // 5. Index rebased onto head, then a normal record.
    let state = repo.state().clone();
    let proj = project(&state);
    let idx = wd::index_for(
        &Target {
            state: &state,
            proj: &proj,
            frontier: repo.head_frontier(),
        },
        wd::case_insensitive(&repo.dir)?,
    );
    idx.save(&repo.dir)?;
    durable::crash_point(C_INDEX);
    let mut buf: Vec<u8> = Vec::new();
    let rec = crate::record(&mut repo, None, false, &mut buf, warn)?;
    durable::crash_point(C_RECORD);
    // 5b. Re-create the removed checkpoints, private, on the rescued head
    // (R1 repair). Idempotent: an identical checkpoint is not added twice.
    let mut restored = Vec::new();
    let mut seen: Vec<(String, ReplicaId, Option<String>)> = Vec::new();
    for (publ, body) in &m.ckpts {
        let (_, msg, author, note) =
            store::split_ckpt(body).map_err(|w| Error::new(Code::Corrupt, w))?;
        let note = Some(match note {
            Some(n) => format!("{n}; rescued from replica {}", &rid_hex(m.old)[..8]),
            None => format!("rescued from replica {}", &rid_hex(m.old)[..8]),
        });
        if seen.contains(&(msg.clone(), author, note.clone())) {
            continue;
        }
        seen.push((msg.clone(), author, note.clone()));
        let c = hub_core::checkpoint::Checkpoint::from_canonical(
            repo.head_frontier(),
            msg.clone(),
            author,
            note,
        );
        if !repo.checkpoints.iter().any(|x| *x == c) {
            store::append_record(&repo.dir.join("checkpoints.log"), &store::encode_ckpt(&c))?;
            repo.checkpoints.add(c.clone());
        }
        restored.push((msg, *publ, repo.index().short(&c.id())));
    }
    durable::crash_point(C_CKPTS_RESTORED);
    // 6. Clear the block and the resolved memory, then mark done.
    let r: BTreeSet<OpId> = m.ops.iter().map(|o| o.id).collect();
    sync::drop_warnings(&repo.dir, |w| {
        w.reason == RejectReason::DuplicateReplica
            || (w.reason == RejectReason::Equivocation
                && w.rejected.is_some_and(|id| r.contains(&id)))
    })?;
    reported::forget(&repo.dir, true, |e| r.contains(&e.op))?;
    durable::crash_point(C_WARNINGS);
    write_durable(&dir.join(DONE), "done\n", None)?;
    durable::crash_point(C_DONE);
    // The marker goes before `current`: a crash in between resumes.
    set_replica(hub, m.new, false)?;
    durable::crash_point(C_UNMARK);
    fs::remove_file(hub.join(DIR).join(CURRENT))?;
    dsync(&hub.join(DIR))?;
    Ok(Outcome {
        recorded: rec.map(|r| r.changes).unwrap_or_default(),
        restored,
        record_lines: String::from_utf8_lossy(&buf)
            .lines()
            .map(str::to_string)
            .collect(),
    })
}

fn plan_j(m: &Manifest, dry: bool, backup: Option<&Path>) -> Vec<(&'static str, J)> {
    vec![
        ("dry_run", J::Bool(dry)),
        ("old_replica", J::s(rid_hex(m.old))),
        (
            "new_replica",
            if dry { J::Null } else { J::s(rid_hex(m.new)) },
        ),
        (
            "rescue",
            J::Arr(m.ops.iter().map(|o| json::id(o.id)).collect()),
        ),
        (
            "files",
            J::Arr(
                m.files
                    .iter()
                    .map(|(c, p)| J::obj(vec![("path", J::s(p)), ("change", J::s(c))]))
                    .collect(),
            ),
        ),
        (
            "backup",
            J::opt(backup.map(|p| J::s(p.display().to_string()))),
        ),
        ("checkpoints", ckpts_j(m)),
    ]
}

fn ckpts_j(m: &Manifest) -> J {
    J::Arr(
        m.ckpts
            .iter()
            .filter_map(|(publ, b)| {
                let (_, msg, author, _) = store::split_ckpt(b).ok()?;
                Some(J::obj(vec![
                    ("message", J::s(msg)),
                    ("author", J::s(rid_hex(author))),
                    ("published", J::Bool(*publ)),
                ]))
            })
            .collect(),
    )
}

/// `barocss-hub reid --rescue [--dry-run] [--offline]`.
pub fn cmd(cwd: &Path, opts: Opts, out: &mut dyn Write, warn: &mut dyn Write) -> Result<(), Error> {
    let hub = std::fs::canonicalize(sync::find_hub(cwd)?)?;
    remote::refuse_bare(&hub, "reid --rescue")?;
    // A dry run is read-only under a shared lock: allowed while watch runs.
    if let Some(pid) = watch::active_watch(&hub)?.filter(|_| !opts.dry_run) {
        return errc(
            Code::Blocked,
            format!("refusing to rescue while `barocss-hub watch` runs (pid {pid}); stop it first"),
        );
    }
    let mode = if opts.dry_run {
        LockMode::Shared
    } else {
        LockMode::Exclusive
    };
    let repo = Repo::open_dir_with(hub.clone(), mode)?;
    for w in &repo.warnings {
        writeln!(warn, "warning: {w}")?;
    }
    let mut resumed = false;
    let (m, ts) = match current_ts(&hub)? {
        Some(ts) => {
            resumed = true;
            let m = read_manifest(&hub.join(DIR).join(&ts).join(MANIFEST))?;
            (m, ts)
        }
        None => {
            // A crash between setting the marker and writing `current`
            // left only the marker (nothing else changed): clear it.
            if !opts.dry_run && marked(&hub) {
                set_replica(&hub, repo.replica_id, false)?;
            }
            let r = rescue_set(&repo)?;
            let dup = has_dup_block(&repo)?;
            if r.is_empty() && !dup {
                drop(repo);
                if opts.json {
                    return json::emit(
                        out,
                        json::doc(
                            "reid",
                            vec![
                                ("dry_run", J::Bool(opts.dry_run)),
                                ("noop", J::Bool(true)),
                                ("rescue", J::Arr(vec![])),
                                ("next", J::Null),
                            ],
                        ),
                    );
                }
                writeln!(
                    out,
                    "nothing to rescue: no rejected or diverged ops and no duplicate-replica block"
                )?;
                return Ok(());
            }
            check_at_head(&repo)?;
            let files = if r.is_empty() {
                Vec::new()
            } else {
                differing(&repo, &r)?
            };
            let opsr: BTreeSet<OpId> = r.clone();
            let private = ckpts_naming(&hub.join("checkpoints.log"), store::CKPT_HEADER, &opsr)?;
            let publ = ckpts_naming(&hub.join("published.log"), sync::PUBLISHED_HEADER, &opsr)?;
            let mut ckpts: Vec<(bool, String)> = private
                .iter()
                .map(|b| (publ.contains(b), b.clone()))
                .collect();
            for b in &publ {
                if !private.contains(b) {
                    ckpts.push((true, b.clone()));
                }
            }
            let mut new = core::new_replica_id();
            while new == repo.replica_id {
                new = core::new_replica_id();
            }
            let m = Manifest {
                old: repo.replica_id,
                new,
                floor: repo.store.keys().map(|o| o.counter).max().unwrap_or(0),
                ops: repo
                    .store
                    .values()
                    .filter(|o| r.contains(&o.id))
                    .cloned()
                    .collect(),
                files,
                ckpts,
            };
            (m, now_ts())
        }
    };
    let rdir = hub.join(DIR).join(&ts);
    let bdir = rdir.join(FILES);
    if opts.dry_run {
        drop(repo);
        if opts.json {
            // A fresh plan has no directory yet: name the parent.
            let shown = if resumed { bdir.clone() } else { hub.join(DIR) };
            let mut f = plan_j(&m, true, (!m.files.is_empty()).then_some(shown.as_path()));
            f.push(("resumed", J::Bool(resumed)));
            f.push(("noop", J::Bool(false)));
            f.push((
                "next",
                J::s("run `barocss-hub reid --rescue` to apply this plan"),
            ));
            return json::emit(out, json::doc("reid", f));
        }
        writeln!(out, "dry run: nothing is changed")?;
        print_plan(&m, resumed, &bdir, out)?;
        writeln!(out, "run `barocss-hub reid --rescue` to apply this plan")?;
        return Ok(());
    }
    // Stage 0: backup + manifest + current.
    if !resumed {
        sweep_orphans(&hub);
        fs::create_dir_all(&rdir)?;
        backup(&repo.root(), &rdir, &m.files)?;
        durable::crash_point(C_BACKUP);
        write_durable(&rdir.join(MANIFEST), &encode_manifest(&m), None)?;
        durable::crash_point(C_MANIFEST);
    }
    // The marker before `current` (and again on a resume): from here on a
    // pre-#49 binary refuses the repository.
    set_replica(&hub, repo.replica_id, true)?;
    durable::crash_point(C_MARKED);
    if !resumed {
        write_durable(&hub.join(DIR).join(CURRENT), &format!("{ts}\n"), None)?;
    }
    drop(repo);
    if !opts.json {
        print_plan(&m, resumed, &bdir, out)?;
    }
    rewrite_stages(&hub, &m)?;
    let o = record_stages(&hub, &rdir, &m, warn)?;
    if !opts.json {
        for l in &o.record_lines {
            writeln!(out, "{l}")?;
        }
        for (msg, publ, short) in &o.restored {
            writeln!(
                out,
                "re-created checkpoint [{short}] {msg}{}",
                if *publ {
                    " (was published: re-publish it with `barocss-hub publish` if wanted)"
                } else {
                    ""
                }
            )?;
        }
        writeln!(
            out,
            "rescued: replica id {} -> {}; the duplicate-replica block is cleared",
            rid_hex(m.old),
            rid_hex(m.new)
        )?;
    }
    // Online confirmation (Wiki 4.13 step 1).
    let mut sync_j = J::Null;
    let mut checkout_j = J::Null;
    let mut next = "run `barocss-hub sync` to send the rescued edits".to_string();
    if !opts.offline && remote::resolve(cwd, None).is_ok() {
        let mut sbuf: Vec<u8> = Vec::new();
        let r = remote::sync(
            cwd,
            None,
            opts.json || !std::io::IsTerminal::is_terminal(&std::io::stdin()),
            remote::cli_trust_mode(false, opts.json),
            warn,
        );
        match r {
            Ok(s) => {
                sync::print_report(&s, &mut sbuf, warn)?;
                s.remember(warn)?;
                let newrej = s.new_rejected();
                next = if newrej.is_empty() {
                    "nothing else to do".into()
                } else {
                    "the remote rejected more ops; run `barocss-hub reid --rescue` again".into()
                };
                if !opts.json {
                    out.write_all(&sbuf)?;
                }
                checkout_j = auto_checkout(&hub, out, warn, opts.json)?;
                sync_j = J::obj(vec![
                    ("ok", J::Bool(true)),
                    (
                        "exit",
                        J::n(if newrej.is_empty() {
                            0u64
                        } else {
                            json::EXIT_REJECTED as u64
                        }),
                    ),
                    ("rejected", json::rejections_j(&s.rej)),
                    ("message", J::Null),
                ]);
            }
            Err(e) => {
                writeln!(
                    warn,
                    "warning: rescue is complete, but the remote could not be reached to confirm it ({}); run `barocss-hub sync` later",
                    e.0
                )?;
                sync_j = J::obj(vec![
                    ("ok", J::Bool(false)),
                    ("exit", J::n(json::EXIT_ERROR as u64)),
                    ("rejected", J::Arr(vec![])),
                    ("message", J::s(&e.0)),
                ]);
            }
        }
    }
    if opts.json {
        let backed = rdir.join(FILES);
        let mut f = plan_j(&m, false, backed.exists().then_some(backed.as_path()));
        f.push(("recorded", json::changes(&o.recorded)));
        f.push(("resumed", J::Bool(resumed)));
        f.push(("noop", J::Bool(false)));
        f.push(("sync", sync_j));
        f.push(("checkout", checkout_j));
        f.push((
            "restored_checkpoints",
            J::Arr(
                o.restored
                    .iter()
                    .map(|(msg, publ, short)| {
                        J::obj(vec![
                            ("message", J::s(msg)),
                            ("short", J::s(short)),
                            ("was_published", J::Bool(*publ)),
                        ])
                    })
                    .collect(),
            ),
        ));
        f.push(("next", J::s(&next)));
        return json::emit(out, json::doc("reid", f));
    }
    writeln!(out, "next: {next}")?;
    Ok(())
}

/// After an online rescue (Open #2, decided): check out head, but only if
/// the working tree is still exactly what the rescue recorded.
fn auto_checkout(
    hub: &Path,
    out: &mut dyn Write,
    warn: &mut dyn Write,
    json: bool,
) -> Result<J, Error> {
    #[cfg(debug_assertions)]
    if let Some(p) = std::env::var_os("HUB_TEST_RESCUE_EDIT_AFTER_SYNC") {
        fs::write(hub.parent().unwrap().join(p), b"edited after the rescue\n")?;
    }
    let mut repo = Repo::open_dir_with(hub.to_path_buf(), LockMode::Exclusive)?;
    let idx = wd::Index::load(&repo.dir)?;
    if idx.base == repo.head_frontier() {
        return Ok(J::obj(vec![
            ("done", J::Bool(false)),
            ("reason", J::s("already at head")),
        ]));
    }
    let mut ws = Vec::new();
    let w = wd::scan(&repo.root(), &idx, &mut ws)?;
    if !wd::diff(&idx, &w).is_empty() {
        let why = "the working directory changed since the rescue recorded it; run `barocss-hub checkout` when ready";
        if !json {
            writeln!(out, "note: not checked out: {why}")?;
        }
        return Ok(J::obj(vec![
            ("done", J::Bool(false)),
            ("reason", J::s(why)),
        ]));
    }
    let mut buf = Vec::new();
    let c = crate::checkout(&mut repo, None, false, &mut buf, warn)?;
    if !json {
        writeln!(
            out,
            "checked out head {} (the working directory was unchanged since the rescue)",
            c.short
        )?;
    }
    Ok(J::obj(vec![
        ("done", J::Bool(true)),
        ("version", J::s(&c.short)),
        ("written", J::n(c.written)),
        ("removed", J::n(c.removed)),
    ]))
}

fn print_plan(m: &Manifest, resumed: bool, bdir: &Path, out: &mut dyn Write) -> Result<(), Error> {
    if resumed {
        writeln!(out, "resuming an interrupted rescue")?;
    }
    writeln!(out, "rescue: replica id {} -> a fresh id", rid_hex(m.old))?;
    if m.ops.is_empty() {
        writeln!(
            out,
            "rescue: no ops to re-author (only the duplicate-replica block is cleared)"
        )?;
    } else {
        let ids: Vec<String> = m.ops.iter().map(|o| store::fmt_id(o.id)).collect();
        writeln!(
            out,
            "rescue: {} op(s) removed and re-authored by effect: {}",
            ids.len(),
            ids.join(", ")
        )?;
    }
    for (c, p) in &m.files {
        writeln!(out, "  {c}: {p}")?;
    }
    for (publ, b) in &m.ckpts {
        if let Ok((_, msg, ..)) = store::split_ckpt(b) {
            writeln!(
                out,
                "  checkpoint '{msg}'{}: removed with the ops, re-created (private) on the rescued head",
                if *publ { " (published)" } else { "" }
            )?;
        }
    }
    if m.files.iter().any(|(c, _)| c != "deleted") {
        writeln!(
            out,
            "rescue: backup of the changed files: {}",
            bdir.display()
        )?;
    }
    Ok(())
}

/// For tests: the rescue directories of `.hub/` `hub`.
pub fn dirs(hub: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(hub.join(DIR))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Issue #54: the rescue manifest carries a `TextEdit` as its `T` record.
#[cfg(test)]
mod text_manifest_tests {
    use super::*;

    #[test]
    fn manifest_round_trips_a_text_edit() {
        let d = std::env::temp_dir().join(format!("hub54-manifest-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        let te = crate::wire::tests::golden_text_op();
        let m = Manifest {
            old: ReplicaId(7),
            new: ReplicaId(9),
            floor: 12,
            ops: vec![te.clone()],
            files: vec![],
            ckpts: vec![],
        };
        let p = d.join("manifest");
        fs::write(&p, encode_manifest(&m)).unwrap();
        let back = read_manifest(&p).unwrap();
        assert_eq!(back.ops, vec![te.clone()]);
        assert!(fs::read_to_string(&p)
            .unwrap()
            .contains(&format!("op {}", store::encode_op(&te))));
        let _ = fs::remove_dir_all(&d);
    }
}
