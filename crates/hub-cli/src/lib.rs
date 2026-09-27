//! `hub` CLI library (Issue #16): a single local replica persisted in `.hub/`.
//!
//! [`Repo`] owns storage and replay; [`run`] implements the commands so they
//! can be tested without spawning a process. `main.rs` is a thin wrapper.

pub mod blobs;
pub mod store;
pub mod sync;
pub mod wd;

use hub_core::checkpoint::{Checkpoint, Checkpoints, ResolveError, VersionId, VersionIndex};
use hub_core::projection::{project, short_id, Projection};
use hub_core::tree::{
    evaluate, is_ancestor_or_self, valid_name, BlobId, Conflict, Kind, NodeId, Op, Payload,
    Replica, ReplicaId, State, ROOT,
};
use hub_core::version::{Store, Version};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use store::{LockMode, RepoLock, StoreError};
use wd::{Change, Entry, Index, PlanOpts, Target};

pub const HUB_DIR: &str = ".hub";

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Self {
        Error(e.to_string())
    }
}

/// Message of the error produced when stdout's reader went away (EPIPE,
/// e.g. `hub log | head -1`); `main` exits quietly on it.
pub const BROKEN_PIPE: &str = "broken pipe";

impl Error {
    pub fn is_broken_pipe(&self) -> bool {
        self.0 == BROKEN_PIPE
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            return Error(BROKEN_PIPE.into());
        }
        Error(format!("i/o error: {e}"))
    }
}

pub(crate) fn err<T>(s: impl Into<String>) -> Result<T, Error> {
    Err(Error(s.into()))
}

/// An opened repository: the replayed replica plus checkpoints.
pub struct Repo {
    pub dir: PathBuf,
    pub replica_id: ReplicaId,
    pub replica: Replica,
    pub store: Store,
    pub checkpoints: Checkpoints,
    /// Human-readable notes from opening (torn-tail recovery).
    pub warnings: Vec<String>,
    /// `.hub/lock`, held for the lifetime of the `Repo`.
    _lock: RepoLock,
}

impl Repo {
    fn paths(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
        (
            dir.join("replica"),
            dir.join("ops.log"),
            dir.join("checkpoints.log"),
        )
    }

    /// Create `.hub/` under `root`.
    pub fn init(root: &Path) -> Result<Repo, Error> {
        let dir = root.join(HUB_DIR);
        if dir.exists() {
            return err(format!("already a hub repository: {}", dir.display()));
        }
        fs::create_dir_all(&dir)?;
        let (rp, op, cp) = Self::paths(&dir);
        store::write_atomic(&op, format!("{}\n", store::OPLOG_HEADER).as_bytes())?;
        store::write_atomic(&cp, format!("{}\n", store::CKPT_HEADER).as_bytes())?;
        let rid = hub_core::sync::new_replica_id();
        // Replica file last: its presence marks a complete init.
        store::write_atomic(
            &rp,
            format!("{}\n{:032x}\n", store::REPLICA_HEADER, rid.0).as_bytes(),
        )?;
        Self::open_dir(dir)
    }

    /// Find `.hub/` in `start` or an ancestor and open it for writing
    /// (exclusive lock).
    pub fn discover(start: &Path) -> Result<Repo, Error> {
        Self::discover_with(start, LockMode::Exclusive)
    }

    /// Find `.hub/` in `start` or an ancestor and open it under `mode`.
    pub fn discover_with(start: &Path, mode: LockMode) -> Result<Repo, Error> {
        let mut cur = Some(start);
        while let Some(c) = cur {
            if c.join(HUB_DIR).is_dir() {
                return Self::open_dir_with(c.join(HUB_DIR), mode);
            }
            cur = c.parent();
        }
        err("not a hub repository (no .hub/ here or in any parent); run `hub init`")
    }

    /// Open and replay a `.hub/` directory for writing (exclusive lock).
    pub fn open_dir(dir: PathBuf) -> Result<Repo, Error> {
        Self::open_dir_with(dir, LockMode::Exclusive)
    }

    /// Open and replay a `.hub/` directory. The lock is taken before any
    /// file is read and held until the `Repo` is dropped, so a writer's
    /// replay -> mint -> append is atomic with respect to other processes.
    pub fn open_dir_with(dir: PathBuf, mode: LockMode) -> Result<Repo, Error> {
        let lock = RepoLock::acquire(&dir, mode)
            .map_err(|e| Error(format!("cannot lock {}: {e}", dir.join("lock").display())))?;
        let (rp, op, cp) = Self::paths(&dir);
        let rtext = fs::read_to_string(&rp)
            .map_err(|e| Error(format!("cannot read {}: {e}", rp.display())))?;
        let replica_id = match rtext.lines().collect::<Vec<_>>().as_slice() {
            [h, id] if *h == store::REPLICA_HEADER && id.len() == 32 => {
                ReplicaId(u128::from_str_radix(id, 16).map_err(|_| Error("bad replica id".into()))?)
            }
            _ => return err(format!("corrupt {}", rp.display())),
        };
        let mut warnings = Vec::new();
        let ops = store::read_log(&op, store::OPLOG_HEADER)?;
        if ops.truncated_tail {
            warnings.push(format!(
                "recovered: dropped a truncated trailing record from {}",
                op.display()
            ));
        }
        let mut replica = Replica::new();
        let mut st = Store::new();
        for (i, body) in ops.records.iter().enumerate() {
            let o = store::decode_op(body).map_err(|why| StoreError::Corrupt {
                file: op.clone(),
                line: i + 2,
                why,
            })?;
            // A repeated OpId must carry the identical op: a different
            // payload/deps under one id is equivocation (e.g. two unlocked
            // writers minting the same counter) and is corruption. An exact
            // repeat is idempotent and skipped.
            if let Some(prev) = st.get(&o.id) {
                if *prev != o {
                    return Err(StoreError::Corrupt {
                        file: op.clone(),
                        line: i + 2,
                        why: format!(
                            "duplicate op id {} with a different op",
                            store::fmt_id(o.id)
                        ),
                    }
                    .into());
                }
                continue;
            }
            st.insert(o.id, o.clone());
            replica.receive(o);
        }
        if replica.pending_len() != 0 {
            return err(format!(
                "corrupt {}: {} op(s) with missing deps",
                op.display(),
                replica.pending_len()
            ));
        }
        let cks = store::read_log(&cp, store::CKPT_HEADER)?;
        if cks.truncated_tail {
            warnings.push(format!(
                "recovered: dropped a truncated trailing record from {}",
                cp.display()
            ));
        }
        let mut checkpoints = Checkpoints::new();
        for (i, body) in cks.records.iter().enumerate() {
            let c = store::decode_ckpt(body, &st).map_err(|why| StoreError::Corrupt {
                file: cp.clone(),
                line: i + 2,
                why,
            })?;
            checkpoints.add(c);
        }
        Ok(Repo {
            dir,
            replica_id,
            replica,
            store: st,
            checkpoints,
            warnings,
            _lock: lock,
        })
    }

    pub fn state(&self) -> &State {
        self.replica.state()
    }

    pub fn head(&self) -> Version {
        Version::from_ops(&self.store, self.store.keys().copied().collect())
            .expect("store is closed")
    }

    /// Persist then apply an op (any author; used by tests and `mint`).
    pub fn append_op(&mut self, op: Op) -> Result<(), Error> {
        store::append_record(&self.dir.join("ops.log"), &store::encode_op(&op))?;
        self.store.insert(op.id, op.clone());
        self.replica.receive(op);
        Ok(())
    }

    /// Build (not persist) a local op over the current frontier with a
    /// Lamport counter.
    pub fn prepare(&self, payload: Payload) -> Op {
        let deps: BTreeSet<_> = self.head().frontier(&self.store).into_iter().collect();
        let counter = 1 + self.store.keys().map(|o| o.counter).max().unwrap_or(1);
        Op {
            id: NodeId::new(counter, self.replica_id.0),
            deps,
            payload,
        }
    }

    /// Author a local op over the current frontier with a Lamport counter.
    pub fn mint(&mut self, payload: Payload) -> Result<Op, Error> {
        let op = self.prepare(payload);
        self.append_op(op.clone())?;
        Ok(op)
    }

    /// The working directory: the parent of `.hub/`.
    pub fn root(&self) -> PathBuf {
        self.dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn blobs(&self) -> Result<blobs::Blobs, Error> {
        Ok(blobs::Blobs::open(&self.dir)?)
    }

    /// Sorted frontier of the head.
    pub fn head_frontier(&self) -> Vec<NodeId> {
        let mut f = self.head().frontier(&self.store);
        f.sort();
        f
    }

    pub fn commit(&mut self, message: &str) -> Result<Checkpoint, Error> {
        let c = Checkpoint::new(&self.store, &self.head(), message, self.replica_id, None);
        store::append_record(&self.dir.join("checkpoints.log"), &store::encode_ckpt(&c))?;
        self.checkpoints.add(c.clone());
        Ok(c)
    }

    pub fn index(&self) -> VersionIndex {
        let mut ix = VersionIndex::with_checkpoints(&self.store, &self.checkpoints);
        ix.insert(&self.store, &self.head());
        ix
    }
}

// ---------- path resolution ----------

fn norm(path: &str) -> Result<String, Error> {
    let parts: Vec<&str> = path
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.contains(&"..") {
        return err(format!("invalid path '{path}': '..' is not supported"));
    }
    Ok(parts.join("/"))
}

fn lookup(proj: &Projection, path: &str) -> Result<NodeId, Error> {
    let p = norm(path)?;
    proj.entries
        .get(&p)
        .copied()
        .ok_or_else(|| Error(format!("no such path: '{path}'")))
}

/// Resolve a not-yet-existing path to (parent dir, new name).
fn resolve_new(state: &State, proj: &Projection, path: &str) -> Result<(NodeId, String), Error> {
    let p = norm(path)?;
    if p.is_empty() {
        return err(format!("invalid path '{path}'"));
    }
    if proj.entries.contains_key(&p) {
        return err(format!("already exists: '{p}'"));
    }
    let (dir, name) = p.rsplit_once('/').unwrap_or(("", &p));
    if !valid_name(name) {
        return err(format!("invalid name '{name}'"));
    }
    let parent = lookup(proj, dir)?;
    if state[&parent].kind == Kind::File {
        return err(format!("not a directory: '{dir}'"));
    }
    Ok((parent, name.to_string()))
}

// ---------- rendering ----------

fn render_tree(state: &State, proj: &Projection, under: NodeId) -> Vec<String> {
    let base = &proj.paths[&under];
    let mut out: Vec<String> = proj
        .paths
        .iter()
        .filter(|(id, p)| **id != under && is_ancestor_or_self(state, under, **id) && !p.is_empty())
        .map(|(id, p)| {
            let rel = if base.is_empty() {
                p.clone()
            } else {
                p[base.len() + 1..].to_string()
            };
            if state[id].kind == Kind::Directory {
                format!("{rel}/")
            } else {
                rel
            }
        })
        .collect();
    out.sort();
    out
}

fn render_conflicts(proj: &Projection, conflicts: &BTreeSet<Conflict>) -> Vec<String> {
    let name = |n: NodeId| {
        proj.paths
            .get(&n)
            .cloned()
            .unwrap_or_else(|| format!("<trashed {}>", short_id(n)))
    };
    let mut out = Vec::new();
    for c in &proj.collisions {
        out.push(format!(
            "name-collision: '{}' projected as '{}' (winner {})",
            c.name,
            name(c.node),
            short_id(c.winner)
        ));
    }
    for c in conflicts {
        out.push(match c {
            Conflict::InvalidOp { op } => format!("invalid-op: {}", short_id(*op)),
            Conflict::Cycle {
                winner,
                loser,
                node,
            } => format!(
                "cycle: move {} of {} skipped (winner {})",
                short_id(*loser),
                name(*node),
                short_id(*winner)
            ),
            Conflict::SameNodeMove {
                winner,
                loser,
                node,
            } => format!(
                "same-node-move: {} of {} overwritten by {}",
                short_id(*loser),
                name(*node),
                short_id(*winner)
            ),
            Conflict::Revived { node, delete, mv } => format!(
                "revived: {} kept live by {} despite delete {}",
                name(*node),
                short_id(*mv),
                short_id(*delete)
            ),
            Conflict::Content {
                node,
                winner,
                loser,
                winner_blob,
                loser_blob,
            } => format!(
                "content: {} set by {} (blob {}) overwritten by {} (blob {})",
                name(*node),
                short_id(*loser),
                &loser_blob.hex()[..8],
                short_id(*winner),
                &winner_blob.hex()[..8]
            ),
            Conflict::EditDeleted { node, edit, delete } => format!(
                "edit-deleted: content edit {} of {} kept on trashed node (delete {})",
                short_id(*edit),
                name(*node),
                short_id(*delete)
            ),
        });
    }
    out
}

fn render_long(state: &State, proj: &Projection) -> Vec<String> {
    proj.entries
        .iter()
        .filter(|(p, _)| !p.is_empty())
        .map(|(p, n)| match state[n].kind {
            Kind::File => {
                let b = proj.blobs.get(n).copied().unwrap_or_else(BlobId::empty);
                format!("{} {} {p}", short_id(*n), &b.hex()[..12])
            }
            _ => format!("{} {:12} {p}/", short_id(*n), "-"),
        })
        .collect()
}

fn resolve(repo: &Repo, ix: &VersionIndex, id: &str) -> Result<Version, Error> {
    ix.resolve_version(&repo.store, id).map_err(|e| match e {
        ResolveError::InvalidPrefix => {
            Error(format!("invalid version id '{id}' (need 4-64 hex digits)"))
        }
        ResolveError::NotFound => Error(format!("unknown version '{id}'")),
        ResolveError::Ambiguous(ids) => Error(format!(
            "ambiguous version '{id}': {}",
            ids.iter()
                .map(|i| ix.short(i))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    })
}

fn not_at_head(repo: &Repo, idx: &Index) -> Error {
    let ix = repo.index();
    Error(format!(
        "working directory is at version {}, not head {}; run `hub checkout` to return to head first",
        ix.short(&VersionId::of_frontier(&idx.base)),
        ix.short(&VersionId::of_frontier(&repo.head_frontier()))
    ))
}

/// Load the index, distrusting it when its base names ops the log does not
/// have (e.g. a torn tail was dropped): then every on-disk path is treated
/// as unrecorded (never clean), so no checkout deletes or overwrites it
/// without `--force`.
fn load_index(repo: &Repo, warn: &mut dyn Write) -> Result<Index, Error> {
    let idx = Index::load(&repo.dir)?;
    if idx.base.iter().any(|id| !repo.store.contains_key(id)) {
        writeln!(
            warn,
            "warning: the index names ops missing from the log; treating all working files as unrecorded"
        )?;
        return Ok(Index::default());
    }
    Ok(idx)
}

/// Refuse while an interrupted checkout's marker exists.
fn refuse_if_interrupted(repo: &Repo) -> Result<(), Error> {
    if wd::interrupted(&repo.dir)?.is_some() {
        return err(
            "a checkout was interrupted; the working directory is partly updated. \
             Run `hub checkout` to finish it before recording or changing the tree",
        );
    }
    Ok(())
}

fn print_warnings(warn: &mut dyn Write, ws: &[String]) -> Result<(), Error> {
    for w in ws {
        writeln!(warn, "warning: {w}")?;
    }
    Ok(())
}

/// mkdir/touch/mv/rm: append `payload` and materialize its effect in the
/// working directory. The plan is computed first, so a refused working-dir
/// update appends nothing.
fn mutate(repo: &mut Repo, payload: Payload, warn: &mut dyn Write) -> Result<(), Error> {
    let root = repo.root();
    refuse_if_interrupted(repo)?;
    let idx = load_index(repo, warn)?;
    if idx.base != repo.head_frontier() {
        return Err(not_at_head(repo, &idx));
    }
    let op = repo.prepare(payload);
    let ev = evaluate(repo.store.values().chain(std::iter::once(&op)));
    let proj = project(&ev.state);
    let mut ws = Vec::new();
    let w = wd::scan(&root, &idx, &mut ws)?;
    let plan = wd::plan(
        &Target {
            state: &ev.state,
            proj: &proj,
            frontier: vec![op.id],
        },
        &idx,
        &w,
        &repo.blobs()?,
        &PlanOpts {
            force: false,
            restore_missing: false,
            case_insensitive: wd::case_insensitive(&repo.dir)?,
        },
    )?;
    repo.append_op(op)?;
    ws.extend(wd::apply(plan, &root, &repo.dir)?);
    print_warnings(warn, &ws)
}

fn record(
    repo: &mut Repo,
    msg: Option<&str>,
    out: &mut dyn Write,
    warn: &mut dyn Write,
) -> Result<(), Error> {
    let root = repo.root();
    refuse_if_interrupted(repo)?;
    wd::sweep_tmp(&repo.dir)?;
    let blobs = repo.blobs()?;
    let swept = blobs.sweep_temps()?;
    if swept > 0 {
        writeln!(
            warn,
            "warning: removed {swept} incomplete blob temp file(s) left by an interrupted write"
        )?;
    }
    let idx = load_index(repo, warn)?;
    if idx.base != repo.head_frontier() {
        return Err(not_at_head(repo, &idx));
    }
    let mut ws = Vec::new();
    let w = wd::scan(&root, &idx, &mut ws)?;
    print_warnings(warn, &ws)?;
    let changes = wd::diff(&idx, &w);
    if changes.is_empty() {
        writeln!(out, "nothing to record")?;
        return Ok(());
    }
    let old = |p: &str| idx.entries[p].0;
    let mut created: BTreeMap<String, NodeId> = BTreeMap::new();
    let parent_of = |created: &BTreeMap<String, NodeId>, path: &str| -> (NodeId, String) {
        let (d, n) = path.rsplit_once('/').unwrap_or(("", path));
        let pn = if d.is_empty() {
            ROOT
        } else {
            created.get(d).copied().unwrap_or_else(|| idx.entries[d].0)
        };
        (pn, n.to_string())
    };
    // Store every new blob before appending any op, so a crash can never
    // leave an op that names a missing blob (or a half-recorded change).
    let mut stored: BTreeMap<&str, BlobId> = BTreeMap::new();
    for c in &changes {
        if let Change::Added(p) | Change::Modified(p) = c {
            if matches!(w[p], Entry::File { .. }) {
                let bytes = fs::read(root.join(p))?;
                let b = blobs.put(&bytes).map_err(|e| Error(e.to_string()))?;
                stored.insert(p, b);
            }
        }
    }
    let put = |path: &str| -> Result<BlobId, Error> { Ok(stored[path]) };
    // 1. Creates, parents first (a path sorts before its descendants).
    for c in &changes {
        let Change::Added(p) = c else { continue };
        let (parent, name) = parent_of(&created, p);
        let is_dir = matches!(w[p], Entry::Dir);
        let kind = if is_dir { Kind::Directory } else { Kind::File };
        let op = repo.mint(Payload::Create { kind, parent, name })?;
        created.insert(p.clone(), op.id);
        if !is_dir {
            let blob = put(p)?;
            if blob != BlobId::empty() {
                repo.mint(Payload::SetContent { node: op.id, blob })?;
            }
        }
    }
    // 2. Moves (before deletes, so a file moved out of a deleted dir survives).
    for c in &changes {
        let Change::Renamed(a, b) = c else { continue };
        let (new_parent, new_name) = parent_of(&created, b);
        repo.mint(Payload::Move {
            node: old(a),
            new_parent,
            new_name,
        })?;
    }
    // 3. Content edits.
    for c in &changes {
        let Change::Modified(p) = c else { continue };
        let blob = put(p)?;
        repo.mint(Payload::SetContent { node: old(p), blob })?;
    }
    // 4. Deletes, top-most only (a Delete trashes the whole subtree).
    let deleted: BTreeSet<&str> = changes
        .iter()
        .filter_map(|c| match c {
            Change::Deleted(p) => Some(p.as_str()),
            _ => None,
        })
        .collect();
    for p in &deleted {
        let covered = p.match_indices('/').any(|(i, _)| deleted.contains(&p[..i]));
        if !covered {
            repo.mint(Payload::Delete { node: old(p) })?;
        }
    }
    // New index from the head projection plus the scanned stats.
    let state = repo.state().clone();
    let proj = project(&state);
    let mut nidx = Index {
        base: repo.head_frontier(),
        ..Index::default()
    };
    for (p, n) in &proj.entries {
        if p.is_empty() {
            continue;
        }
        let e = match (state[n].kind, w.get(p)) {
            (Kind::File, we) => {
                let blob = proj.blobs.get(n).copied().unwrap_or_else(BlobId::empty);
                match we {
                    Some(Entry::File {
                        blob: wb,
                        size,
                        mtime,
                    }) if *wb == blob => Entry::File {
                        blob,
                        size: *size,
                        mtime: *mtime,
                    },
                    _ => {
                        writeln!(warn, "warning: '{p}' differs from the recorded tree")?;
                        Entry::File {
                            blob,
                            size: 0,
                            mtime: 0,
                        }
                    }
                }
            }
            _ => Entry::Dir,
        };
        nidx.entries.insert(p.clone(), (*n, e));
    }
    for p in w.keys() {
        if !proj.entries.contains_key(p) {
            writeln!(warn, "warning: '{p}' is not in the recorded tree")?;
        }
    }
    nidx.save(&repo.dir)?;
    writeln!(out, "recorded {} change(s):", changes.len())?;
    for l in wd::render_changes(&changes, &idx, &w) {
        writeln!(out, "  {l}")?;
    }
    if let Some(m) = msg {
        let c = repo.commit(m)?;
        let ix = repo.index();
        writeln!(out, "[{}] {}", ix.short(&c.id()), c.message)?;
    }
    Ok(())
}

fn checkout(
    repo: &mut Repo,
    id: Option<&str>,
    force: bool,
    out: &mut dyn Write,
    warn: &mut dyn Write,
) -> Result<(), Error> {
    let root = repo.root();
    let blobs = repo.blobs()?;
    blobs.sweep_temps()?;
    wd::sweep_tmp(&repo.dir)?;
    let ix = repo.index();
    let v = match (id, wd::interrupted(&repo.dir)?) {
        (Some(id), _) => resolve(repo, &ix, id)?,
        (None, Some(f)) => match Version::downclose(&repo.store, f.iter().copied()) {
            Ok(v) => {
                writeln!(
                    warn,
                    "warning: resuming interrupted checkout of {}",
                    ix.short(&VersionId::of_frontier(&f))
                )?;
                v
            }
            Err(_) => {
                writeln!(
                    warn,
                    "warning: interrupted checkout names unknown ops; checking out head"
                )?;
                repo.head()
            }
        },
        (None, None) => repo.head(),
    };
    let ev = evaluate(v.resolve(&repo.store));
    let proj = project(&ev.state);
    let idx = match load_index(repo, warn) {
        Ok(i) => i,
        Err(e) if force => {
            writeln!(warn, "warning: {e}; ignoring the index (--force)")?;
            Index::default()
        }
        Err(e) => return Err(e),
    };
    let mut ws = Vec::new();
    let w = wd::scan(&root, &idx, &mut ws)?;
    let frontier = v.frontier(&repo.store);
    let plan = wd::plan(
        &Target {
            state: &ev.state,
            proj: &proj,
            frontier: frontier.clone(),
        },
        &idx,
        &w,
        &blobs,
        &PlanOpts {
            force,
            restore_missing: true,
            case_insensitive: wd::case_insensitive(&repo.dir)?,
        },
    )?;
    let (written, removed) = plan.counts();
    ws.extend(wd::apply(plan, &root, &repo.dir)?);
    print_warnings(warn, &ws)?;
    let vid = VersionId::of_frontier(&frontier);
    writeln!(
        out,
        "checked out {} ({written} written, {removed} removed)",
        ix.short(&vid)
    )?;
    let mut f = frontier;
    f.sort();
    if f != repo.head_frontier() {
        writeln!(
            out,
            "note: not at head; `hub record` and mkdir/touch/mv/rm need `hub checkout` (head) first"
        )?;
    }
    Ok(())
}

pub const HELP: &str = "\
hub - CRDT-native source control (experimental, single local replica)

Usage: hub <command> [args]

The working directory is the directory containing .hub/ (minus .hub/).

Commands:
  init                 create .hub/ in the current directory
  record [-m <msg>]    record working-directory changes as ops (-m: also checkpoint)
  checkout [--force] [<short-id>]  write a version (default: head) to the working directory
  mkdir <path>         create a directory (op + working directory)
  touch <path>         create an empty file (op + working directory)
  mv <from> <to>       move/rename a node, <to> must not exist (op + working directory)
  rm <path>            delete a node, moves it to TRASH (op + working directory)
  ls [-l] [path]       list the projected head tree (-l: node ids, blob ids)
  status               head, conflicts, unrecorded working-directory changes, tree
  commit -m <msg>      checkpoint the current head (local, private)
  log                  list checkpoints with short version ids, newest first
  show [-l] <short-id> print the tree of a version (hex prefix, >= 4 digits)
  sync <path>          exchange ops, blobs and published checkpoints with another
                       local repo (log only; run `hub checkout` afterwards)
  publish <short-id|message>  mark a local checkpoint published (sync sends it)
  warnings             list sync warnings (D-008) and pending ops
  reid                 give this repo a fresh replica id (after copying a .hub/)
  help, --help         show this help
";

/// Run one command in `cwd`. Output goes to `out`; warnings to `warn`.
pub fn run(
    args: &[String],
    cwd: &Path,
    out: &mut dyn Write,
    warn: &mut dyn Write,
) -> Result<(), Error> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let usage = |c: &str| {
        let syn = HELP
            .lines()
            .map(str::trim_start)
            .find(|l| l.split_whitespace().next() == Some(c))
            .and_then(|l| l.split("  ").next())
            .unwrap_or(c);
        Error(format!("usage: hub {}  (see hub --help)", syn.trim_end()))
    };
    if matches!(a.as_slice(), [] | ["help"] | ["--help"] | ["-h"]) {
        out.write_all(HELP.as_bytes())?;
        return Ok(());
    }
    if a[0] == "init" {
        if a.len() != 1 {
            return Err(usage("init"));
        }
        let r = Repo::init(cwd)?;
        writeln!(
            out,
            "initialized hub repository in {} (replica {:032x})",
            r.dir.display(),
            r.replica_id.0
        )?;
        return Ok(());
    }
    if a[0] == "sync" {
        return match a.as_slice() {
            ["sync", p] => sync::sync_cmd(cwd, p, out, warn),
            _ => Err(usage("sync")),
        };
    }
    let known = [
        "publish", "warnings", "reid", "mkdir", "touch", "mv", "rm", "ls", "status", "commit",
        "log", "show", "record", "checkout",
    ];
    if !known.contains(&a[0]) {
        return err(format!("unknown command '{}' (see hub --help)", a[0]));
    }
    // Mutating commands hold an exclusive lock from replay to append (and,
    // for record/checkout, across the working-directory update and index
    // write); read-only commands take a shared lock.
    let mode = if matches!(
        a[0],
        "mkdir" | "touch" | "mv" | "rm" | "commit" | "record" | "checkout" | "publish" | "reid"
    ) {
        LockMode::Exclusive
    } else {
        LockMode::Shared
    };
    let mut repo = Repo::discover_with(cwd, mode)?;
    for w in &repo.warnings {
        writeln!(warn, "warning: {w}")?;
    }
    let state = repo.state().clone();
    let proj = project(&state);
    match a.as_slice() {
        ["mkdir", p] | ["touch", p] => {
            let (parent, name) = resolve_new(&state, &proj, p)?;
            let kind = if a[0] == "mkdir" {
                Kind::Directory
            } else {
                Kind::File
            };
            mutate(&mut repo, Payload::Create { kind, parent, name }, warn)?;
        }
        ["mv", from, to] => {
            let node = lookup(&proj, from)?;
            if node == hub_core::tree::ROOT {
                return err("cannot move the root");
            }
            let (new_parent, new_name) = resolve_new(&state, &proj, to)?;
            if is_ancestor_or_self(&state, node, new_parent) {
                return err(format!("cannot move '{from}' into itself"));
            }
            mutate(
                &mut repo,
                Payload::Move {
                    node,
                    new_parent,
                    new_name,
                },
                warn,
            )?;
        }
        ["rm", p] => {
            let node = lookup(&proj, p)?;
            if node == hub_core::tree::ROOT {
                return err("cannot remove the root");
            }
            mutate(&mut repo, Payload::Delete { node }, warn)?;
        }
        ["record"] => record(&mut repo, None, out, warn)?,
        ["record", "-m", msg] => record(&mut repo, Some(msg), out, warn)?,
        ["checkout", rest @ ..] => {
            let force = rest.contains(&"--force");
            let ids: Vec<&str> = rest.iter().copied().filter(|x| *x != "--force").collect();
            match ids.as_slice() {
                [] => checkout(&mut repo, None, force, out, warn)?,
                [id] if !id.starts_with('-') => checkout(&mut repo, Some(id), force, out, warn)?,
                _ => return Err(usage("checkout")),
            }
        }
        ["ls", "-l"] => {
            for l in render_long(&state, &proj) {
                writeln!(out, "{l}")?;
            }
        }
        ["ls"] | ["ls", _] => {
            let under = match a.get(1) {
                Some(p) => lookup(&proj, p)?,
                None => hub_core::tree::ROOT,
            };
            for l in render_tree(&state, &proj, under) {
                writeln!(out, "{l}")?;
            }
        }
        ["status"] => {
            let ix = repo.index();
            let head = repo.head();
            let hid = hub_core::checkpoint::version_id(&repo.store, &head);
            writeln!(
                out,
                "head {} ({} ops, {} checkpoints, {} published)",
                ix.short(&hid),
                repo.store.len(),
                repo.checkpoints.len(),
                sync::read_published(&repo)?.len()
            )?;
            let ev = evaluate(repo.store.values());
            let cs = render_conflicts(&proj, &ev.conflicts);
            if cs.is_empty() {
                writeln!(out, "conflicts: none")?;
            } else {
                writeln!(out, "conflicts:")?;
                for c in cs {
                    writeln!(out, "  {c}")?;
                }
            }
            let (sw, npend) = sync::warnings_view(&repo)?;
            if !sw.is_empty() {
                writeln!(out, "sync warnings:")?;
                for w in &sw {
                    writeln!(out, "  {}", sync::render_warning(w))?;
                }
            }
            if npend > 0 {
                writeln!(
                    out,
                    "pending ops: {npend} (missing deps; retried on the next sync)"
                )?;
            }
            if wd::interrupted(&repo.dir)?.is_some() {
                writeln!(out, "interrupted checkout: run `hub checkout` to finish it")?;
            }
            let idx = load_index(&repo, warn)?;
            if idx.base != repo.head_frontier() {
                writeln!(
                    out,
                    "working directory at version {} (not head)",
                    ix.short(&VersionId::of_frontier(&idx.base))
                )?;
                let n = sync::behind_by(&repo, &idx.base);
                if n > 0 {
                    writeln!(
                        out,
                        "working directory is behind head by {n} op(s); `hub checkout` brings it up to date"
                    )?;
                }
            }
            let mut ws = Vec::new();
            let w = wd::scan(&repo.root(), &idx, &mut ws)?;
            print_warnings(warn, &ws)?;
            let changes = wd::diff(&idx, &w);
            if changes.is_empty() {
                writeln!(out, "working directory: clean")?;
            } else {
                writeln!(out, "unrecorded changes:")?;
                for l in wd::render_changes(&changes, &idx, &w) {
                    writeln!(out, "  {l}")?;
                }
            }
            writeln!(out, "tree:")?;
            for l in render_tree(&state, &proj, hub_core::tree::ROOT) {
                writeln!(out, "  {l}")?;
            }
        }
        ["commit", "-m", msg] => {
            let c = repo.commit(msg)?;
            let ix = repo.index();
            writeln!(out, "[{}] {}", ix.short(&c.id()), c.message)?;
        }
        ["log"] => {
            let ix = repo.index();
            let published = sync::read_published(&repo)?;
            let mut list: Vec<&Checkpoint> = repo.checkpoints.iter().collect();
            for c in &published {
                if !list.contains(&c) {
                    list.push(c);
                }
            }
            for c in list.iter().rev() {
                if published.contains(c) {
                    writeln!(out, "{} [published] {}", ix.short(&c.id()), c.message)?;
                } else {
                    writeln!(out, "{} {}", ix.short(&c.id()), c.message)?;
                }
            }
        }
        ["publish", what] => {
            let ix = repo.index();
            let published = sync::read_published(&repo)?;
            let mut cands: Vec<&Checkpoint> = repo
                .checkpoints
                .iter()
                .filter(|c| c.message == *what)
                .collect();
            let lw = what.to_ascii_lowercase();
            if cands.is_empty() && lw.len() >= 4 && lw.bytes().all(|b| b.is_ascii_hexdigit()) {
                cands = repo
                    .checkpoints
                    .iter()
                    .filter(|c| c.id().hex().starts_with(&lw))
                    .collect();
            }
            let mut uniq: Vec<&Checkpoint> = Vec::new();
            for c in cands {
                if !uniq.contains(&c) {
                    uniq.push(c);
                }
            }
            match uniq.as_slice() {
                [] => return err(format!("no local checkpoint matches '{what}'")),
                [c] if published.contains(c) => {
                    writeln!(
                        out,
                        "already published: [{}] {}",
                        ix.short(&c.id()),
                        c.message
                    )?;
                }
                [c] => {
                    sync::append_published(&repo, c)?;
                    writeln!(out, "published [{}] {}", ix.short(&c.id()), c.message)?;
                }
                many => {
                    return err(format!(
                        "ambiguous checkpoint '{what}': {}; publish by exact message",
                        many.iter()
                            .map(|c| format!("[{}] {}", ix.short(&c.id()), c.message))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                }
            }
        }
        ["warnings"] => {
            let (sw, _) = sync::warnings_view(&repo)?;
            let pend = sync::read_pending(&repo.dir)?;
            if sw.is_empty() && pend.is_empty() {
                writeln!(out, "no warnings")?;
            }
            for w in &sw {
                writeln!(out, "{}", sync::render_warning(w))?;
            }
            for (o, p) in &pend {
                writeln!(
                    out,
                    "pending op {} from peer {} (missing deps)",
                    store::fmt_id(o.id),
                    &format!("{:032x}", p.0)[..8]
                )?;
            }
        }
        ["reid"] => sync::reid(&mut repo, out)?,
        ["show", id] | ["show", "-l", id] => {
            let long = a.len() == 3;
            let ix = repo.index();
            let v = resolve(&repo, &ix, id)?;
            let ev = evaluate(v.resolve(&repo.store));
            let p = project(&ev.state);
            let vid = hub_core::checkpoint::version_id(&repo.store, &v);
            writeln!(out, "version {}", vid.hex())?;
            let msgs: BTreeMap<_, Vec<&str>> =
                repo.checkpoints.iter().fold(BTreeMap::new(), |mut m, c| {
                    m.entry(c.id()).or_default().push(c.message.as_str());
                    m
                });
            for m in msgs.get(&vid).into_iter().flatten() {
                writeln!(out, "    {m}")?;
            }
            let lines = if long {
                render_long(&ev.state, &p)
            } else {
                render_tree(&ev.state, &p, hub_core::tree::ROOT)
            };
            for l in lines {
                writeln!(out, "{l}")?;
            }
        }
        _ => return Err(usage(a[0])),
    }
    Ok(())
}
