//! `hub` CLI library (Issue #16): a single local replica persisted in `.hub/`.
//!
//! [`Repo`] owns storage and replay; [`run`] implements the commands so they
//! can be tested without spawning a process. `main.rs` is a thin wrapper.

pub mod store;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use hub_core::checkpoint::{Checkpoint, Checkpoints, ResolveError, VersionIndex};
use hub_core::projection::{project, short_id, Projection};
use hub_core::tree::{
    evaluate, is_ancestor_or_self, valid_name, Conflict, Kind, NodeId, Op, Payload, Replica,
    ReplicaId, State,
};
use hub_core::version::{Store, Version};

use store::StoreError;

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

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(format!("i/o error: {e}"))
    }
}

fn err<T>(s: impl Into<String>) -> Result<T, Error> {
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
}

fn new_replica_id(root: &Path) -> ReplicaId {
    let mut h = Sha256::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    h.update(now.to_be_bytes());
    h.update(std::process::id().to_be_bytes());
    h.update(root.to_string_lossy().as_bytes());
    let d = h.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    // Non-zero: replica 0 is reserved for ROOT/TRASH.
    ReplicaId(u128::from_be_bytes(b).max(1))
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
        let rid = new_replica_id(root);
        // Replica file last: its presence marks a complete init.
        store::write_atomic(
            &rp,
            format!("{}\n{:032x}\n", store::REPLICA_HEADER, rid.0).as_bytes(),
        )?;
        Self::open_dir(dir)
    }

    /// Find `.hub/` in `start` or an ancestor and open it.
    pub fn discover(start: &Path) -> Result<Repo, Error> {
        let mut cur = Some(start);
        while let Some(c) = cur {
            if c.join(HUB_DIR).is_dir() {
                return Self::open_dir(c.join(HUB_DIR));
            }
            cur = c.parent();
        }
        err("not a hub repository (no .hub/ here or in any parent); run `hub init`")
    }

    /// Open and replay a `.hub/` directory.
    pub fn open_dir(dir: PathBuf) -> Result<Repo, Error> {
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

    /// Author a local op over the current frontier with a Lamport counter.
    pub fn mint(&mut self, payload: Payload) -> Result<Op, Error> {
        let deps: BTreeSet<_> = self.head().frontier(&self.store).into_iter().collect();
        let counter = 1 + self.store.keys().map(|o| o.counter).max().unwrap_or(1);
        let op = Op {
            id: NodeId::new(counter, self.replica_id.0),
            deps,
            payload,
        };
        self.append_op(op.clone())?;
        Ok(op)
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
        });
    }
    out
}

pub const HELP: &str = "\
hub - CRDT-native source control (experimental, single local replica)

Usage: hub <command> [args]

Commands:
  init                 create .hub/ in the current directory
  mkdir <path>         create a directory
  touch <path>         create an empty file node
  mv <from> <to>       move/rename a node (<to> must not exist)
  rm <path>            delete a node (moves it to TRASH)
  ls [path]            list the projected tree (optionally under <path>)
  status               tree, conflicts, name collisions and head version
  commit -m <msg>      checkpoint the current version (local, private)
  log                  list checkpoints with short version ids, newest first
  show <short-id>      print the tree of a version (hex prefix, >= 4 digits)
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
    let usage = |c: &str| Error(format!("usage: hub {c}  (see hub --help)"));
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
    let known = [
        "mkdir", "touch", "mv", "rm", "ls", "status", "commit", "log", "show",
    ];
    if !known.contains(&a[0]) {
        return err(format!("unknown command '{}' (see hub --help)", a[0]));
    }
    let mut repo = Repo::discover(cwd)?;
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
            repo.mint(Payload::Create { kind, parent, name })?;
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
            repo.mint(Payload::Move {
                node,
                new_parent,
                new_name,
            })?;
        }
        ["rm", p] => {
            let node = lookup(&proj, p)?;
            if node == hub_core::tree::ROOT {
                return err("cannot remove the root");
            }
            repo.mint(Payload::Delete { node })?;
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
                "head {} ({} ops, {} checkpoints)",
                ix.short(&hid),
                repo.store.len(),
                repo.checkpoints.len()
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
            let list: Vec<&Checkpoint> = repo.checkpoints.iter().collect();
            for c in list.iter().rev() {
                writeln!(out, "{} {}", ix.short(&c.id()), c.message)?;
            }
        }
        ["show", id] => {
            let ix = repo.index();
            let v = ix.resolve_version(&repo.store, id).map_err(|e| match e {
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
            })?;
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
            for l in render_tree(&ev.state, &p, hub_core::tree::ROOT) {
                writeln!(out, "{l}")?;
            }
        }
        _ => return Err(usage(a[0])),
    }
    Ok(())
}
