//! `hub` CLI library (Issue #16): a single local replica persisted in `.hub/`.
//!
//! [`Repo`] owns storage and replay; [`run`] implements the commands so they
//! can be tested without spawning a process. `main.rs` is a thin wrapper.

pub mod blobs;
pub mod durable;
pub mod ignore;
pub mod json;
pub mod migrate;
pub mod remote;
pub mod session;
pub mod snapshot;
pub mod store;
pub mod sync;
pub mod trust;
pub mod watch;
pub mod wd;
pub mod wire;

use hub_core::checkpoint::{
    Checkpoint, Checkpoints, FrontierCheck, ResolveError, VersionId, VersionIndex,
};
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

/// A command error: the human message and its code, chosen where the
/// error is constructed (Issue #37 repair; `--json` `error.code`).
#[derive(Debug)]
/// The third field is true for an error the remote reported in a wire
/// `Error` frame (Issue #43; `--json` adds `"remote": true`).
pub struct Error(pub String, pub Code, pub bool);

/// Stable error codes (schema 1, Wiki CLI-JSON). Never derived from text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    /// Bad arguments or an unknown command.
    Usage,
    /// No `.hub/` here or in any parent.
    NotARepository,
    /// `init` in a repository, or `mkdir`/`touch`/`mv` onto an existing path.
    AlreadyExists,
    /// A path argument that cannot name a node (`..`, empty, bad name).
    InvalidPath,
    /// No node at the path (also a missing directory argument).
    NoSuchPath,
    NotAFile,
    NotADirectory,
    /// A version id that is not 4-64 hex digits.
    BadVersion,
    /// A well-formed version id (or checkpoint) that matches nothing.
    NotFound,
    /// A prefix or message matching several versions or checkpoints.
    Ambiguous,
    NoSuchBlob,
    BadBlobId,
    /// Another process holds the repository or watch lock.
    Locked,
    /// The working directory is behind head: run `checkout` first.
    Behind,
    /// The working directory is pinned to an old version.
    Pinned,
    /// A checkout was interrupted: run `checkout` to finish it.
    Interrupted,
    /// Working-directory paths collide with unmaterialized paths (D-019).
    Blocked,
    /// The working-directory update would clobber unrecorded or ignored data.
    Refused,
    /// Ops under our own replica id that we never authored (D-021).
    DuplicateReplica,
    /// A damaged `.hub/` file or blob.
    Corrupt,
    Io,
    /// Wire protocol violation: malformed or unexpected frame (Issue #43).
    Protocol,
    /// No common wire protocol version with the remote.
    VersionMismatch,
    /// A wire limit (frame, chunk, list, blob, record size) was exceeded.
    Limit,
    /// The remote sent nothing within the step timeout.
    Timeout,
    /// A feature or URL scheme that is not negotiated or not implemented.
    Unsupported,
    /// Refused by the server: path confinement, a forced command, or a
    /// write to a read-only server.
    Forbidden,
    /// The command needs a working directory; this is a bare repository.
    Bare,
    /// First contact with an ssh remote this user never added or confirmed
    /// on this machine, without a terminal to ask and without `--yes`
    /// (Issue #45).
    UntrustedRemote,
    /// Anything else.
    Error,
}

impl Code {
    pub const ALL: &'static [Code] = &[
        Code::Usage,
        Code::NotARepository,
        Code::AlreadyExists,
        Code::InvalidPath,
        Code::NoSuchPath,
        Code::NotAFile,
        Code::NotADirectory,
        Code::BadVersion,
        Code::NotFound,
        Code::Ambiguous,
        Code::NoSuchBlob,
        Code::BadBlobId,
        Code::Locked,
        Code::Behind,
        Code::Pinned,
        Code::Interrupted,
        Code::Blocked,
        Code::Refused,
        Code::DuplicateReplica,
        Code::Corrupt,
        Code::Io,
        Code::Protocol,
        Code::VersionMismatch,
        Code::Limit,
        Code::Timeout,
        Code::Unsupported,
        Code::Forbidden,
        Code::Bare,
        Code::UntrustedRemote,
        Code::Error,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Code::Usage => "usage",
            Code::NotARepository => "not-a-repository",
            Code::AlreadyExists => "already-exists",
            Code::InvalidPath => "invalid-path",
            Code::NoSuchPath => "no-such-path",
            Code::NotAFile => "not-a-file",
            Code::NotADirectory => "not-a-directory",
            Code::BadVersion => "bad-version",
            Code::NotFound => "not-found",
            Code::Ambiguous => "ambiguous",
            Code::NoSuchBlob => "no-such-blob",
            Code::BadBlobId => "bad-blob-id",
            Code::Locked => "locked",
            Code::Behind => "behind",
            Code::Pinned => "pinned",
            Code::Interrupted => "interrupted",
            Code::Blocked => "blocked",
            Code::Refused => "refused",
            Code::DuplicateReplica => "duplicate-replica",
            Code::Corrupt => "corrupt",
            Code::Io => "io",
            Code::Protocol => "protocol",
            Code::VersionMismatch => "version-mismatch",
            Code::Limit => "limit",
            Code::Timeout => "timeout",
            Code::Unsupported => "unsupported",
            Code::Forbidden => "forbidden",
            Code::Bare => "bare",
            Code::UntrustedRemote => "untrusted-remote",
            Code::Error => "error",
        }
    }
}

impl Error {
    pub fn new(code: Code, msg: impl Into<String>) -> Error {
        Error(msg.into(), code, false)
    }
    /// An error reported by the remote end of a sync (wire `Error` frame).
    pub fn remote(code: Code, msg: impl Into<String>) -> Error {
        Error(msg.into(), code, true)
    }
    pub fn is_remote(&self) -> bool {
        self.2
    }
    pub fn code(&self) -> Code {
        self.1
    }
    /// Same code, message prefixed with `<p>: `.
    pub fn prefix(self, p: &str) -> Error {
        Error(format!("{p}: {}", self.0), self.1, self.2)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Self {
        let code = match e {
            StoreError::Io(_) => Code::Io,
            StoreError::Corrupt { .. } => Code::Corrupt,
        };
        Error::new(code, e.to_string())
    }
}

/// The [`Code`] of a blob-store error.
pub(crate) fn blob_error(e: blobs::BlobError) -> Error {
    let code = match e {
        blobs::BlobError::Missing(_) => Code::NoSuchBlob,
        blobs::BlobError::Corrupt { .. } => Code::Corrupt,
        blobs::BlobError::Io(_) => Code::Io,
    };
    Error::new(code, e.to_string())
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
            return Error::new(Code::Io, BROKEN_PIPE);
        }
        Error::new(Code::Io, format!("i/o error: {e}"))
    }
}

static FAST_EXIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// For a process that runs one command and exits (`main.rs`, Issue #33):
/// the opened repository is not freed at the end of [`run`] (at 1M ops
/// dropping it costs ~0.2 s); the OS reclaims it, and the repo lock (an
/// open file) is released at exit as before. Never set by in-process
/// callers (tests), which would otherwise keep the lock.
pub fn set_fast_exit() {
    FAST_EXIT.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn fast_exit() -> bool {
    FAST_EXIT.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn err<T>(s: impl Into<String>) -> Result<T, Error> {
    Err(Error::new(Code::Error, s.into()))
}

/// `Err` with an explicit [`Code`].
pub(crate) fn errc<T>(code: Code, s: impl Into<String>) -> Result<T, Error> {
    Err(Error::new(code, s.into()))
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
    /// Lock mode held (snapshots are written only under `Exclusive`).
    mode: LockMode,
    /// End of the committed `ops.log` (Issue #33).
    pos: store::LogPos,
    /// Ops covered by the current valid `.hub/snapshot` (0 = none).
    snap_ops: u64,
    /// Issue #32 per-command memos, keyed by `store.len()` (the store only
    /// grows) and cleared by every append: head frontier + max counter,
    /// and the `FrontierCheck::scan` result.
    head_memo: std::cell::RefCell<Option<(usize, HeadMemo)>>,
    fast_memo: std::cell::Cell<Option<(usize, bool)>>,
}

#[derive(Clone)]
struct HeadMemo {
    frontier: Vec<NodeId>,
    max_counter: Option<u64>,
}

/// Case count for the in-crate CLI proptests (Issue #34 test time: each
/// case spawns whole repositories, so 512 cases cost ~400 s serial). The
/// default is `default`; `PROPTEST_CASES=<n>` restores any larger count,
/// e.g. `PROPTEST_CASES=512` for the pre-#34 depth.
#[cfg(test)]
pub(crate) fn test_cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
thread_local! {
    /// Test-only switch (Issue #32): run every command on the pre-#32 paths
    /// (full `evaluate` for conflicts and for record/mutate planning, the
    /// single-map index, `Version::from_ops` head, a fresh `FrontierCheck`
    /// per decode, the store-cloning `warnings_view`).
    pub static ORACLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the pre-#32 oracle paths are on (always false outside tests).
pub(crate) fn oracle() -> bool {
    #[cfg(test)]
    {
        ORACLE.with(|o| o.get())
    }
    #[cfg(not(test))]
    {
        false
    }
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
        Self::init_opts(root, false)
    }

    /// [`Repo::init`]; `bare` also writes the `.hub/bare` marker (D-042),
    /// before the replica file that marks a complete init.
    pub fn init_opts(root: &Path, bare: bool) -> Result<Repo, Error> {
        let dir = root.join(HUB_DIR);
        if dir.exists() {
            return errc(
                Code::AlreadyExists,
                format!("already a hub repository: {}", dir.display()),
            );
        }
        fs::create_dir_all(&dir)?;
        let (rp, op, cp) = Self::paths(&dir);
        store::write_atomic(&op, format!("{}\n", store::OPLOG_HEADER).as_bytes())?;
        store::write_atomic(&cp, format!("{}\n", store::CKPT_HEADER).as_bytes())?;
        // Format v1 (Issue #33, Appendix A.2.7): watch.lock, format. (No
        // `peers` since #43, D-041.)
        if bare {
            store::write_atomic(&dir.join(remote::BARE_FILE), b"")?;
        }
        fs::File::create(dir.join(watch::LOCK_FILE))?;
        store::write_atomic(&dir.join("format"), store::FORMAT_V1.as_bytes())?;
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
        errc(
            Code::NotARepository,
            "not a hub repository (no .hub/ here or in any parent); run `barocss-hub init`",
        )
    }

    /// Open and replay a `.hub/` directory for writing (exclusive lock).
    pub fn open_dir(dir: PathBuf) -> Result<Repo, Error> {
        Self::open_dir_with(dir, LockMode::Exclusive)
    }

    /// Open and replay a `.hub/` directory. The lock is taken before any
    /// file is read and held until the `Repo` is dropped, so a writer's
    /// replay -> mint -> append is atomic with respect to other processes.
    pub fn open_dir_with(dir: PathBuf, mode: LockMode) -> Result<Repo, Error> {
        Self::open_dir_opts(dir, mode, None)
    }

    /// [`Repo::open_dir_with`] waiting at most `wait` for the lock (Issue
    /// #43, `serve`), else `locked`.
    pub fn open_dir_opts(
        dir: PathBuf,
        mode: LockMode,
        wait: Option<std::time::Duration>,
    ) -> Result<Repo, Error> {
        let acquire = |m: LockMode| {
            match wait {
                Some(w) => RepoLock::acquire_timeout(&dir, m, w),
                None => RepoLock::acquire(&dir, m),
            }
            .map_err(|e| {
                Error::new(
                    Code::Locked,
                    format!("cannot lock {}: {e}", dir.join("lock").display()),
                )
            })
        };
        let mut lock = acquire(mode)?;
        let mut warnings = Vec::new();
        let mut migrated = None;
        // Format v1 (Issue #33): migrate a v0 repository, or finish an
        // interrupted migration, under the exclusive lock.
        let (fmt, _) = migrate::read_replica(&dir)?;
        if fmt == migrate::Format::V0 || migrate::needs_finish(&dir) {
            if mode != LockMode::Exclusive {
                drop(lock);
                lock = acquire(LockMode::Exclusive)?;
            }
            match migrate::read_replica(&dir)?.0 {
                migrate::Format::V0 => {
                    let (r, p) = migrate::migrate(&dir)?;
                    // Step 5, still under the exclusive lock.
                    let mut covered = 0;
                    if snapshot::due(0, p.ops) {
                        if let Some(img) =
                            snapshot::encode(&r, &p, &head_of(&r.ops().collect::<Vec<_>>()))
                        {
                            match snapshot::write(&dir, &img) {
                                Ok(()) => covered = p.ops,
                                Err(e) => warnings.push(format!(
                                    "could not write {}: {e}",
                                    dir.join(snapshot::FILE).display()
                                )),
                            }
                        }
                    }
                    migrated = Some((r, p, covered));
                    warnings.push(format!("migrated {} to on-disk format v1", dir.display()));
                }
                migrate::Format::V1 => migrate::finish(&dir)?,
            }
            if mode != LockMode::Exclusive {
                drop(lock);
                lock = acquire(mode)?;
            }
        }
        if mode == LockMode::Exclusive {
            remote::migrate_peers(&dir)?;
        }
        let (fmt, rid) = migrate::read_replica(&dir)?;
        if fmt != migrate::Format::V1 {
            return errc(
                Code::Corrupt,
                format!("corrupt {}", dir.join("replica").display()),
            );
        }
        migrate::check_format(&dir)?;
        let replica_id = ReplicaId(rid);
        let (_, op, cp) = Self::paths(&dir);
        if !store::LOCKING_AVAILABLE {
            warnings.push(store::NO_LOCK_WARNING.to_string());
        }
        // Snapshot (Appendix A.2.3): validated, else ignored with a warning.
        let mut base: Option<(Replica, store::LogPos)> = None;
        let mut snap_head: Option<snapshot::Head> = None;
        let mut mig_covered = 0;
        let was_migrated = migrated.is_some();
        if let Some((r, p, c)) = migrated.take() {
            mig_covered = c;
            base = Some((r, p));
        } else if !snapshot::disabled() {
            match snapshot::load(&dir) {
                Ok(Some((r, p, h))) => {
                    base = Some((r, p));
                    snap_head = Some(h);
                }
                Ok(None) => {}
                Err(why) => {
                    warnings.push(format!(
                        "ignoring {} ({why}); replaying {}",
                        dir.join(snapshot::FILE).display(),
                        op.display()
                    ));
                    if mode == LockMode::Exclusive {
                        let _ = fs::remove_file(dir.join(snapshot::FILE));
                    }
                }
            }
        }
        let (snap_pos, replica0) = match base {
            Some((r, p)) => (Some(p), Some(r)),
            None => (None, None),
        };
        let full = || -> Result<(store::OpLogRead, Store, Replica), Error> {
            let all = store::read_oplog(&op, None)?;
            let (st, r) = replay_records(&op, &all.records, &|i| all.lines[i], None)?;
            Ok((all, st, r))
        };
        let mut new_ids: Vec<NodeId> = Vec::new();
        let tail =
            |new_ids: &mut Vec<NodeId>| -> Result<(store::OpLogRead, Store, Replica), Error> {
                let t = store::read_oplog(&op, snap_pos.as_ref())?;
                let n0 = replica0.as_ref().map_or(0, |r| r.len());
                let (st, r) = replay_records(&op, &t.records, &|i| t.lines[i], replica0)?;
                // Ids replayed on top of the snapshot (duplicates skipped).
                if st.len() > n0 {
                    for body in &t.records {
                        if let Ok(o) = store::decode_op(body) {
                            new_ids.push(o.id);
                        }
                    }
                }
                Ok((t, st, r))
            };
        let (ops, st, replica, snap_ops) = if snap_pos.is_some() {
            match tail(&mut new_ids) {
                Ok((t, st, r)) => {
                    let n = if was_migrated {
                        mig_covered
                    } else {
                        t.pos.ops.min(snap_pos.as_ref().unwrap().ops)
                    };
                    (t, st, r, n)
                }
                // Damage after the snapshot: a full read gives the same
                // error (line numbers) as without a snapshot.
                Err(_) => {
                    snap_head = None;
                    let (t, st, r) = full()?;
                    (t, st, r, 0)
                }
            }
        } else {
            let (t, st, r) = full()?;
            (t, st, r, 0)
        };
        if ops.truncated_tail {
            warnings.push(format!(
                "recovered: dropped a truncated trailing record from {}",
                op.display()
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
        let check = FrontierCheck::new(&st);
        let fast = check.is_fast();
        for (i, body) in cks.records.iter().enumerate() {
            let c = store::decode_ckpt(body, &check).map_err(|why| StoreError::Corrupt {
                file: cp.clone(),
                line: i + 2,
                why,
            })?;
            checkpoints.add(c);
        }
        let st_len = st.len();
        // Head memo from the snapshot plus the tail (Issue #33): the store
        // is closed, so F(store ∪ tail) = (F ∪ ids(tail)) − deps(tail).
        let head_memo = match snap_head {
            Some(h) if !oracle() => {
                let mut f: BTreeSet<NodeId> = h.frontier.into_iter().collect();
                let mut maxc = h.max_counter;
                new_ids.sort_unstable();
                new_ids.dedup();
                for id in &new_ids {
                    f.insert(*id);
                    maxc = maxc.max(id.counter);
                }
                for id in &new_ids {
                    for d in &st[id].deps {
                        f.remove(d);
                    }
                }
                Some((
                    st_len,
                    HeadMemo {
                        frontier: f.into_iter().collect(),
                        max_counter: Some(maxc),
                    },
                ))
            }
            _ => None,
        };
        let mut repo = Repo {
            dir,
            replica_id,
            replica,
            store: st,
            checkpoints,
            warnings,
            _lock: lock,
            mode,
            pos: ops.pos,
            snap_ops,
            head_memo: std::cell::RefCell::new(head_memo),
            fast_memo: std::cell::Cell::new(Some((st_len, fast))),
        };
        // Step 5 of a migration; otherwise the policy (writers only).
        if let Some(w) = repo.maybe_snapshot() {
            repo.warnings.push(w);
        }
        Ok(repo)
    }

    /// Write `.hub/snapshot` if the policy says so (Appendix A.2.3). Only
    /// under the exclusive lock; a failure is a warning, never an error.
    pub fn maybe_snapshot(&mut self) -> Option<String> {
        if self.mode != LockMode::Exclusive || !snapshot::due(self.snap_ops, self.pos.ops) {
            return None;
        }
        let m = self.head_memo();
        let head = snapshot::Head {
            frontier: m.frontier,
            max_counter: m.max_counter?,
        };
        let image = snapshot::encode(&self.replica, &self.pos, &head)?;
        match snapshot::write(&self.dir, &image) {
            Ok(()) => {
                self.snap_ops = self.pos.ops;
                None
            }
            Err(e) => Some(format!(
                "could not write {}: {e}",
                self.dir.join(snapshot::FILE).display()
            )),
        }
    }

    pub fn state(&self) -> &State {
        self.replica.state()
    }

    pub fn head(&self) -> Version {
        Version::from_ops(&self.store, self.store.keys().copied().collect())
            .expect("store is closed")
    }

    /// Head frontier and greatest counter, memoized per store size (Issue
    /// #32). Frontier = ids that are no op's dep (the store is closed), in
    /// `<` order: the same list as `head().frontier(&store)`.
    fn head_memo(&self) -> HeadMemo {
        let n = self.store.len();
        if !oracle() {
            if let Some((len, m)) = &*self.head_memo.borrow() {
                if *len == n {
                    return m.clone();
                }
            }
        }
        let m = if oracle() {
            HeadMemo {
                frontier: self.head().frontier(&self.store),
                max_counter: self.store.keys().map(|o| o.counter).max(),
            }
        } else {
            let mut deps: Vec<NodeId> = self
                .store
                .values()
                .flat_map(|o| o.deps.iter().copied())
                .collect();
            deps.sort_unstable();
            deps.dedup();
            let frontier = self
                .store
                .keys()
                .filter(|id| deps.binary_search(id).is_err())
                .copied()
                .collect();
            HeadMemo {
                frontier,
                max_counter: self.store.keys().map(|o| o.counter).max(),
            }
        };
        *self.head_memo.borrow_mut() = Some((n, m.clone()));
        m
    }

    /// A [`FrontierCheck`] over the current store, sharing one closed +
    /// Lamport scan per store size (Issue #32).
    pub fn frontier_check(&self) -> FrontierCheck<'_> {
        let n = self.store.len();
        if oracle() {
            return FrontierCheck::new(&self.store);
        }
        match self.fast_memo.get() {
            Some((len, fast)) if len == n => FrontierCheck::with_fast(&self.store, fast),
            _ => {
                let c = FrontierCheck::new(&self.store);
                self.fast_memo.set(Some((n, c.is_fast())));
                c
            }
        }
    }

    fn invalidate(&self) {
        *self.head_memo.borrow_mut() = None;
        self.fast_memo.set(None);
    }

    /// Conflicts of the head: derived from the replayed replica (Issue #32),
    /// equal to `evaluate(store).conflicts`.
    pub fn conflicts(&self) -> BTreeSet<Conflict> {
        if !oracle() {
            if let Some(c) = self.replica.conflicts(&self.store) {
                return c;
            }
        }
        evaluate(self.store.values()).conflicts
    }

    /// State after appending `ops` (greater than every stored op), for
    /// planning before the append: the replica's tail preview, else a full
    /// `evaluate` (Issue #32).
    fn state_with(&self, ops: &[Op]) -> State {
        if !oracle() {
            if let Some(st) = self.replica.state_with_tail(ops) {
                return st;
            }
        }
        evaluate(self.store.values().chain(ops.iter())).state
    }

    /// Persist then apply an op (any author; used by tests and `mint`).
    pub fn append_op(&mut self, op: Op) -> Result<(), Error> {
        let (_, r) = self.append_ops_with(std::iter::once(op), |_| Ok(()));
        r
    }

    /// Persist then apply a batch of ops, refreshing the view once (Issue
    /// #28). Each op is persisted as by [`Repo::append_op`].
    pub fn append_ops(&mut self, ops: impl IntoIterator<Item = Op>) -> Result<(), Error> {
        self.append_ops_with(ops, |_| Ok(())).1
    }

    /// [`Repo::append_ops`] with a hook run before persisting the `i`-th op
    /// (crash/interrupt tests); an error stops the batch. Ops persisted
    /// before an error are still applied. Returns how many were persisted.
    ///
    /// Group commit (Issue #29): all lines are written, then `ops.log` is
    /// synced once. On an error the lines already written are still synced
    /// before returning, so "persisted" means durable.
    pub fn append_ops_with(
        &mut self,
        ops: impl IntoIterator<Item = Op>,
        mut before: impl FnMut(usize) -> Result<(), Error>,
    ) -> (usize, Result<(), Error>) {
        let mut done = Vec::new();
        let mut res = Ok(());
        let mut log = match store::LogAppender::open(&self.dir.join("ops.log")) {
            Ok(l) => l,
            Err(e) => return (0, Err(e.into())),
        };
        for op in ops {
            let r = before(done.len())
                .and_then(|()| log.write(&store::encode_op(&op)).map_err(Error::from));
            if let Err(e) = r {
                res = Err(e);
                break;
            }
            done.push(op);
        }
        if !done.is_empty() {
            durable::crash_point(durable::OPS_APPENDED);
            // One batch (Issue #33): the commit record covers exactly the
            // lines written, also when the append stopped early.
            let pos = match log.commit(&self.pos) {
                Ok(p) => p,
                Err(e) => return (0, Err(e.into())),
            };
            durable::crash_point(durable::COMMIT_WRITTEN);
            if let Err(e) = log.sync() {
                // Nothing of this batch is known durable: apply none of it.
                return (0, Err(e.into()));
            }
            durable::crash_point(durable::OPS_FSYNCED);
            self.pos = pos;
        }
        self.invalidate();
        for op in &done {
            self.store.insert(op.id, op.clone());
        }
        let n = done.len();
        self.replica.receive_all(done);
        (n, res)
    }

    /// Build (not persist) a local op over the current frontier with a
    /// Lamport counter.
    pub fn prepare(&self, payload: Payload) -> Op {
        let deps: BTreeSet<_> = self.head_memo().frontier.into_iter().collect();
        self.prepare_on(deps, payload)
    }

    /// Build a local op over explicit `deps` (ops already in the store).
    pub fn prepare_on(&self, deps: BTreeSet<NodeId>, payload: Payload) -> Op {
        let counter = 1 + self.head_memo().max_counter.unwrap_or(1);
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
        let mut f = self.head_memo().frontier;
        f.sort();
        f
    }

    pub fn commit(&mut self, message: &str) -> Result<Checkpoint, Error> {
        if message.len() > wire::MAX_MESSAGE_BYTES {
            return errc(
                Code::Usage,
                format!(
                    "checkpoint message of {} bytes exceeds the {}-byte limit",
                    message.len(),
                    wire::MAX_MESSAGE_BYTES
                ),
            );
        }
        let c = if oracle() {
            Checkpoint::new(&self.store, &self.head(), message, self.replica_id, None)
        } else {
            Checkpoint::from_canonical(self.head_frontier(), message, self.replica_id, None)
        };
        store::append_record(&self.dir.join("checkpoints.log"), &store::encode_ckpt(&c))?;
        self.checkpoints.add(c.clone());
        Ok(c)
    }

    /// Short-id index: built only by commands that print or resolve a
    /// version id. Head is inserted by its memoized frontier.
    pub fn index(&self) -> VersionIndex {
        #[cfg(test)]
        if oracle() {
            let mut ix = VersionIndex::new_single_map(&self.store, &self.checkpoints);
            ix.insert(&self.store, &self.head());
            return ix;
        }
        let mut ix = VersionIndex::with_checkpoints(&self.store, &self.checkpoints);
        ix.insert_frontier(self.head_frontier());
        ix
    }
}

/// Head frontier (ids no op depends on, `<` order) and max counter of a
/// closed op set given in `<` order.
fn head_of(ops: &[&Op]) -> snapshot::Head {
    let mut deps: Vec<NodeId> = ops.iter().flat_map(|o| o.deps.iter().copied()).collect();
    deps.sort_unstable();
    deps.dedup();
    snapshot::Head {
        frontier: ops
            .iter()
            .map(|o| o.id)
            .filter(|id| deps.binary_search(id).is_err())
            .collect(),
        max_counter: ops.iter().map(|o| o.id.counter).max().unwrap_or(1),
    }
}

/// Decode `records` (op bodies of `file`; record `i` is at line `line_of(i)`)
/// and replay them on top of `base` (a snapshot's replica) or from
/// scratch. Exact repeats are skipped; a repeated id with a different op
/// and ops with missing deps are corruption.
pub(crate) fn replay_records(
    file: &Path,
    records: &[String],
    line_of: &dyn Fn(usize) -> usize,
    base: Option<Replica>,
) -> Result<(Store, Replica), Error> {
    let (mut st, mut replica) = match base {
        Some(r) => (r.ops().map(|o| (o.id, o.clone())).collect::<Store>(), r),
        None => (Store::new(), Replica::new()),
    };
    let mut replay = Vec::with_capacity(records.len());
    for (i, body) in records.iter().enumerate() {
        let o = store::decode_op(body).map_err(|why| StoreError::Corrupt {
            file: file.to_path_buf(),
            line: line_of(i),
            why,
        })?;
        // A repeated OpId must carry the identical op: a different
        // payload/deps under one id is equivocation (e.g. two unlocked
        // writers minting the same counter) and is corruption. An exact
        // repeat is idempotent and skipped.
        if let Some(prev) = st.get(&o.id) {
            if *prev != o {
                return Err(StoreError::Corrupt {
                    file: file.to_path_buf(),
                    line: line_of(i),
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
        replay.push(o);
    }
    // One batch: the view is refreshed once, not per op (Issue #28).
    if !replay.is_empty() || replica.is_empty() {
        replica.receive_all(replay);
    }
    if replica.pending_len() != 0 {
        return errc(
            Code::Corrupt,
            format!(
                "corrupt {}: {} op(s) with missing deps",
                file.display(),
                replica.pending_len()
            ),
        );
    }
    Ok((st, replica))
}

// ---------- path resolution ----------

fn norm(path: &str) -> Result<String, Error> {
    let parts: Vec<&str> = path
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.contains(&"..") {
        return errc(
            Code::InvalidPath,
            format!("invalid path '{path}': '..' is not supported"),
        );
    }
    Ok(parts.join("/"))
}

fn lookup(proj: &Projection, path: &str) -> Result<NodeId, Error> {
    let p = norm(path)?;
    proj.entries
        .get(&p)
        .copied()
        .ok_or_else(|| Error::new(Code::NoSuchPath, format!("no such path: '{path}'")))
}

/// Resolve a not-yet-existing path to (parent dir, new name).
fn resolve_new(state: &State, proj: &Projection, path: &str) -> Result<(NodeId, String), Error> {
    let p = norm(path)?;
    if p.is_empty() {
        return errc(Code::InvalidPath, format!("invalid path '{path}'"));
    }
    if proj.entries.contains_key(&p) {
        return errc(Code::AlreadyExists, format!("already exists: '{p}'"));
    }
    let (dir, name) = p.rsplit_once('/').unwrap_or(("", &p));
    if !valid_name(name) {
        return errc(Code::InvalidPath, format!("invalid name '{name}'"));
    }
    let parent = lookup(proj, dir)?;
    if state[&parent].kind == Kind::File {
        return errc(Code::NotADirectory, format!("not a directory: '{dir}'"));
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
            Conflict::EditRevived { node, edit, delete } => format!(
                "edit-revived: content edit {} revived {} despite delete {}",
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
        ResolveError::InvalidPrefix => Error::new(
            Code::BadVersion,
            format!("invalid version id '{id}' (need 4-64 hex digits)"),
        ),
        ResolveError::NotFound => Error::new(Code::NotFound, format!("unknown version '{id}'")),
        ResolveError::Ambiguous(ids) => Error::new(
            Code::Ambiguous,
            format!(
                "ambiguous version '{id}': {}",
                ids.iter()
                    .map(|i| ix.short(i))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
    })
}

fn not_at_head(repo: &Repo, idx: &Index) -> Error {
    let ix = repo.index();
    let code = match watch::pinned(&repo.dir) {
        Ok(Some(_)) => Code::Pinned,
        _ => Code::Behind,
    };
    Error::new(code, format!(
        "working directory is at version {}, not head {}; run `barocss-hub checkout` to return to head first",
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
        return errc(
            Code::Interrupted,
            "a checkout was interrupted; the working directory is partly updated. \
             Run `barocss-hub checkout` to finish it before recording or changing the tree",
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
fn mutate(
    repo: &mut Repo,
    verb: &'static str,
    payload: Payload,
    warn: &mut dyn Write,
) -> Result<(), Error> {
    let root = repo.root();
    refuse_if_interrupted(repo)?;
    let idx = load_index(repo, warn)?;
    if idx.base != repo.head_frontier() {
        return Err(not_at_head(repo, &idx));
    }
    let op = repo.prepare(payload);
    let state = repo.state_with(std::slice::from_ref(&op));
    let proj = project(&state);
    let mut ws = Vec::new();
    let (w, ignored) = wd::scan_all(&root, &idx, &mut ws)?;
    let plan = wd::plan(
        &Target {
            state: &state,
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
            ignored,
            verb,
        },
    )?;
    repo.append_op(op)?;
    ws.extend(wd::apply(plan, &root, &repo.dir)?);
    print_warnings(warn, &ws)
}

fn w_all_is_dir(root: &Path, p: &str) -> bool {
    fs::symlink_metadata(root.join(p)).is_ok_and(|m| m.is_dir())
}

/// Record the working directory. Ops depend on the index base (the version
/// the user last saw on disk), which is the head for `hub record`. With
/// `allow_behind` (only `hub watch`, Issue #25) the base may be behind head
/// after a sync: the recorded ops are then concurrent with the incoming
/// ones (so a same-file edit is a content conflict, never a silent causal
/// overwrite), and the plan below materializes the merged head over the
/// now fully recorded working tree. Returns what was recorded (`None`:
/// nothing to record).
pub(crate) fn record(
    repo: &mut Repo,
    msg: Option<&str>,
    allow_behind: bool,
    out: &mut dyn Write,
    warn: &mut dyn Write,
) -> Result<Option<Recorded>, Error> {
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
    if !allow_behind && idx.base != repo.head_frontier() {
        return Err(not_at_head(repo, &idx));
    }
    let mut ws = Vec::new();
    // `w_all` keeps untracked ignored paths so the rename plan below never
    // clobbers or deletes them (#24); `w` is what record sees.
    let (w_all, ignored) = wd::scan_all(&root, &idx, &mut ws)?;
    let mut w = w_all.clone();
    w.retain(|p, _| !ignored.contains(p));
    print_warnings(warn, &ws)?;
    let blocked = wd::blocked(&idx, &w);
    if !blocked.is_empty() {
        return Err(Error::new(
            Code::Blocked,
            format!(
                "refusing to record: working-directory paths collide with unmaterialized \
             case-clashing paths (D-019):\n  {}\nresolve by renaming or removing these \
             files, or rename one of the clashing nodes (e.g. in a case-sensitive checkout)",
                wd::render_blocked(&blocked).join("\n  ")
            ),
        ));
    }
    let changes = wd::diff(&idx, &w);
    if changes.is_empty() {
        writeln!(out, "nothing to record")?;
        return Ok(None);
    }
    let old = |p: &str| idx.entries[p].0;
    // Ops are built first and appended only after the working-directory
    // plan succeeds (as in `mutate`), each depending on the previous one:
    // exactly what successive `Repo::mint` calls would produce.
    let mut ops: Vec<Op> = Vec::new();
    let mut mint = |payload: Payload| -> Result<Op, Error> {
        let op = match ops.last() {
            None => repo.prepare_on(idx.base.iter().copied().collect(), payload),
            Some(prev) => Op {
                id: NodeId::new(prev.id.counter + 1, prev.id.replica.0),
                deps: BTreeSet::from([prev.id]),
                payload,
            },
        };
        ops.push(op.clone());
        Ok(op)
    };
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
    // Group commit (Issue #29): one barrier for all of them.
    let mut stored: BTreeMap<&str, BlobId> = BTreeMap::new();
    let mut batch = blobs.batch();
    for c in &changes {
        if let Change::Added(p) | Change::Modified(p) = c {
            if matches!(w[p], Entry::File { .. }) {
                let bytes = fs::read(root.join(p))?;
                let b = batch.add(&bytes).map_err(blob_error)?;
                stored.insert(p, b);
            }
        }
    }
    batch.commit().map_err(blob_error)?;
    let put = |path: &str| -> Result<BlobId, Error> { Ok(stored[path]) };
    // 1. Creates, parents first (a path sorts before its descendants).
    for c in &changes {
        let Change::Added(p) = c else { continue };
        let (parent, name) = parent_of(&created, p);
        let is_dir = matches!(w[p], Entry::Dir);
        let kind = if is_dir { Kind::Directory } else { Kind::File };
        let op = mint(Payload::Create { kind, parent, name })?;
        created.insert(p.clone(), op.id);
        if !is_dir {
            let blob = put(p)?;
            if blob != BlobId::empty() {
                mint(Payload::SetContent { node: op.id, blob })?;
            }
        }
    }
    // 2. Moves (before deletes, so a file moved out of a deleted dir survives).
    for c in &changes {
        let Change::Renamed(a, b) = c else { continue };
        let (new_parent, new_name) = parent_of(&created, b);
        mint(Payload::Move {
            node: old(a),
            new_parent,
            new_name,
        })?;
    }
    // 3. Content edits.
    for c in &changes {
        let Change::Modified(p) = c else { continue };
        let blob = put(p)?;
        mint(Payload::SetContent { node: old(p), blob })?;
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
            mint(Payload::Delete { node: old(p) })?;
        }
    }
    let renamed: BTreeMap<&str, NodeId> = changes
        .iter()
        .filter_map(|c| match c {
            Change::Renamed(a, b) => Some((b.as_str(), old(a))),
            _ => None,
        })
        .collect();
    // Issue #26: the scanned working tree, indexed under the *new* state's
    // nodes at the paths they occupy on disk now. Planning the new head
    // projection against it materializes every projected-path change the
    // user did not make on disk (collisions appearing or disappearing,
    // suffix changes), with checkout's safety rules; everything scanned is
    // recorded, so all of it is clean and nothing unrecorded is clobbered.
    let mut mid = Index {
        base: Vec::new(),
        stamp: idx.stamp,
        ..Index::default()
    };
    for (p, (n, e)) in &idx.entries {
        if *e == Entry::Unmaterialized {
            mid.entries.insert(p.clone(), (*n, *e));
        }
    }
    for (p, we) in &w {
        let node = match (created.get(p), renamed.get(p.as_str()), idx.entries.get(p)) {
            (Some(n), _, _) | (None, Some(n), _) => *n,
            (None, None, Some((n, _))) => *n,
            (None, None, None) => continue,
        };
        let e = match (we, stored.get(p.as_str())) {
            // Changed between scan and read: keep the recorded blob, rehash.
            (Entry::File { blob, .. }, Some(b)) if b != blob => Entry::File {
                blob: *b,
                size: 0,
                mtime: 0,
            },
            _ => *we,
        };
        mid.entries.insert(p.clone(), (node, e));
    }
    let state = repo.state_with(&ops);
    let proj = project(&state);
    let frontier = if oracle() {
        let mut all = repo.store.clone();
        for o in &ops {
            all.insert(o.id, o.clone());
        }
        Version::from_ops(&all, all.keys().copied().collect())
            .expect("store is closed")
            .frontier(&all)
    } else {
        // F(store ∪ ops) = (F(store) ∪ ids(ops)) − deps(ops): the store's
        // own deps are already absent from its frontier (Issue #32, no
        // store clone).
        let deps: BTreeSet<NodeId> = ops.iter().flat_map(|o| o.deps.iter().copied()).collect();
        let f: BTreeSet<NodeId> = repo
            .head_frontier()
            .into_iter()
            .chain(ops.iter().map(|o| o.id))
            .filter(|i| !deps.contains(i))
            .collect();
        f.into_iter().collect()
    };
    let plan = wd::plan(
        &Target {
            state: &state,
            proj: &proj,
            frontier,
        },
        &mid,
        &w_all,
        &blobs,
        &PlanOpts {
            force: false,
            restore_missing: false,
            case_insensitive: wd::case_insensitive(&repo.dir)?,
            verb: "record",
            ignored,
        },
    )?;
    repo.append_ops(ops)?;
    print_warnings(warn, &wd::apply(plan, &root, &repo.dir)?)?;
    writeln!(out, "recorded {} change(s):", changes.len())?;
    for l in wd::render_changes(&changes, &idx, &w) {
        writeln!(out, "  {l}")?;
    }
    let infos = change_infos(&changes, &idx, &w, &created);
    let mut checkpoint = None;
    if let Some(m) = msg {
        let c = repo.commit(m)?;
        let ix = repo.index();
        writeln!(out, "[{}] {}", ix.short(&c.id()), c.message)?;
        checkpoint = Some(c);
    }
    Ok(Some(Recorded {
        changes: infos,
        checkpoint,
    }))
}

/// What one `record` appended (Issue #37, for `--json` and watch events).
pub struct Recorded {
    pub changes: Vec<ChangeInfo>,
    pub checkpoint: Option<Checkpoint>,
}

/// One working-directory change with the node it concerns (Issue #37):
/// `node` is `None` only for an added path not yet recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeInfo {
    /// `added`, `modified`, `deleted` or `renamed`.
    pub kind: &'static str,
    /// The path (for `renamed`, the new path).
    pub path: String,
    /// For `renamed`: the old path.
    pub from: Option<String>,
    pub node: Option<NodeId>,
    pub dir: bool,
}

/// [`ChangeInfo`]s of `changes` (index -> scan); `created` maps added
/// paths to the nodes a record just created for them.
pub(crate) fn change_infos(
    changes: &[Change],
    idx: &Index,
    w: &wd::Scan,
    created: &BTreeMap<String, NodeId>,
) -> Vec<ChangeInfo> {
    let old = |p: &str| idx.entries.get(p).map(|e| e.0);
    let dir_w = |p: &str| matches!(w.get(p), Some(Entry::Dir));
    let dir_i = |p: &str| matches!(idx.entries.get(p), Some((_, Entry::Dir)));
    changes
        .iter()
        .map(|c| match c {
            Change::Added(p) => ChangeInfo {
                kind: "added",
                path: p.clone(),
                from: None,
                node: created.get(p).copied(),
                dir: dir_w(p),
            },
            Change::Modified(p) => ChangeInfo {
                kind: "modified",
                path: p.clone(),
                from: None,
                node: old(p),
                dir: false,
            },
            Change::Deleted(p) => ChangeInfo {
                kind: "deleted",
                path: p.clone(),
                from: None,
                node: old(p),
                dir: dir_i(p),
            },
            Change::Renamed(a, b) => ChangeInfo {
                kind: "renamed",
                path: b.clone(),
                from: Some(a.clone()),
                node: old(a),
                dir: false,
            },
        })
        .collect()
}

/// Issue #30: how to leave a pin when an edit made while pinned conflicts
/// with head (until branches exist). Same text in the README.
pub const PINNED_WAY_OUT: &str =
    "hint: the working directory is pinned to an old version and an edit made \
while pinned conflicts with head. To keep the edit: copy the file(s) aside, run \
`barocss-hub checkout --force` (returns to head and unpins), copy them back, then `barocss-hub record`.";

/// What a checkout wrote (Issue #37, for `--json` and watch events).
pub struct CheckedOut {
    pub version: VersionId,
    pub short: String,
    pub written: usize,
    pub removed: usize,
    pub at_head: bool,
}

pub(crate) fn checkout(
    repo: &mut Repo,
    id: Option<&str>,
    force: bool,
    out: &mut dyn Write,
    warn: &mut dyn Write,
) -> Result<CheckedOut, Error> {
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
    let (w, ignored) = wd::scan_all(&root, &idx, &mut ws)?;
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
            ignored,
            verb: "check out",
        },
    );
    let plan = match plan {
        Err(Error(m, _, _)) if !force && crate::watch::pinned(&repo.dir)?.is_some() => {
            return Err(Error::new(Code::Pinned, format!("{m}\n{PINNED_WAY_OUT}")));
        }
        p => p?,
    };
    let (written, removed) = plan.counts();
    // Issue #25 repair (D-020): pinned iff the materialized version is not
    // head; `apply` writes or clears it before removing the marker.
    let mut plan = plan;
    {
        let mut f = frontier.clone();
        f.sort();
        plan.pin = Some((f != repo.head_frontier()).then_some(f));
    }
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
    let at_head = f == repo.head_frontier();
    if !at_head {
        writeln!(
            out,
            "note: not at head; `barocss-hub record` and mkdir/touch/mv/rm need `barocss-hub checkout` (head) first"
        )?;
    }
    Ok(CheckedOut {
        version: vid,
        short: ix.short(&vid),
        written,
        removed,
        at_head,
    })
}

pub const HELP: &str = "\
barocss-hub - CRDT-native source control (experimental, single local replica)

Usage: barocss-hub <command> [args]

The working directory is the directory containing .hub/ (minus .hub/).

Commands:
  init [--bare [<dir>]]  create .hub/ in the current directory (--bare: a
                       repository without a working directory, for a server)
  record [-m <msg>]    record working-directory changes as ops (-m: also checkpoint)
  checkout [--force] [<short-id>]  write a version (default: head) to the working directory
  mkdir <path>         create a directory (op + working directory)
  touch <path>         create an empty file (op + working directory)
  mv <from> <to>       move/rename a node, <to> must not exist (op + working directory)
  rm <path>            delete a node, moves it to TRASH (op + working directory)
  ls [-l] [path]       list the projected head tree (-l: node ids, blob ids)
  status [--ignored]   head, conflicts, unrecorded working-directory changes, tree
                       (--ignored: also list paths excluded by .hubignore)
  commit -m <msg>      checkpoint the current head (local, private)
  log [--path <path>]  list checkpoints with short version ids, newest first
                       (--path: checkpoints that contain the file at <path>)
  show [-l] <short-id> print the tree of a version (hex prefix, >= 4 digits)
  cat <short-id> <path>  write a file's bytes in that version to stdout
  cat-blob <blob-id>   write a stored blob's bytes (64 hex digits) to stdout
  sync [--yes] [<remote|url>]  exchange ops, blobs and published checkpoints with a remote
                       (name from `remote list`, ssh://host/path, host:path,
                       file:///path or a local path; default: the default remote);
                       log only: run `barocss-hub checkout` afterwards. A configured
                       remote you never added or trusted from this repo asks first
                       (--yes: trust it; no terminal: refused, untrusted-remote)
  remote add [--default] [--exec <path>] [<name>] <url> | list | rm <name> | set-url <name> <url> | trust <name>
                       manage remotes (.hub/remotes, local config, not replicated);
                       --exec: one word of [A-Za-z0-9._/~+-]; trust: mark a
                       remote as used from this machine (~/.config/barocss-hub)
  serve --stdio [--read-only] [--root <dir>] <path>
                       speak the sync protocol on stdin/stdout for one repository
                       (the remote end of ssh; see --from-ssh-original-command)
  publish <short-id|message>  mark a local checkpoint published (sync sends it)
  peer                 removed: use `barocss-hub remote add <name> file:///path`
  watch [--poll <ms>] [--debounce <ms>] [--sync <ms>] [--no-sync]
                       foreground: auto-record working-directory changes,
                       sync with every remote, apply incoming ops; Ctrl-C stops
                       (untrusted ssh remotes are skipped; --no-sync: local only)
  warnings             list sync warnings (D-008) and pending ops
  reid                 give this repo a fresh replica id (after copying a .hub/;
                       run it BEFORE recording in the copy); clears saved
                       duplicate-replica warnings (provisional default)
  help, --help, -h     show this help
  --version, -V        print the version

--json (any command): one JSON document on stdout (schema 1; `watch --json`:
JSON Lines events); errors as JSON on stderr, exit status 1.
";

/// Every dispatchable command name. `run` rejects any other first word, and
/// the tests assert each one appears in [`HELP`] (Issue #35), so the help
/// cannot drift from the dispatch table.
pub const COMMANDS: &[&str] = &[
    "init", "peer", "remote", "serve", "watch", "sync", "publish", "warnings", "reid", "mkdir",
    "touch", "mv", "rm", "ls", "status", "commit", "log", "show", "record", "checkout", "help",
    "cat", "cat-blob",
];

/// `barocss-hub --version` output (from the package version).
pub const VERSION: &str = concat!("barocss-hub ", env!("CARGO_PKG_VERSION"), "\n");

/// Run one command in `cwd`. Output goes to `out`; warnings to `warn`.
///
/// With `--json` (anywhere in `args`, Issue #37) `out` receives exactly one
/// JSON document (schema [`json::SCHEMA`]) and warnings stay as text lines
/// on `warn`; [`main_with`] folds them into the document. `hub watch
/// --json` instead streams JSON Lines events on `out`.
///
/// Help-drift test (Issue #35, `tests/cli.rs`): the test reads this
/// function's *source text* to find every dispatched command word (`a[0]
/// == "x"`, `matches!(a[0], "x" | ..)` and `["x", ..]` slice patterns). A
/// new command must be dispatched in one of those forms here, and listed in
/// [`COMMANDS`] and [`HELP`], or the test fails.
pub fn run(
    args: &[String],
    cwd: &Path,
    out: &mut dyn Write,
    warn: &mut dyn Write,
) -> Result<(), Error> {
    let json = args.iter().any(|x| x == json::FLAG);
    let args: Vec<String> = args.iter().filter(|x| *x != json::FLAG).cloned().collect();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let usage = |c: &str| {
        let syn = HELP
            .lines()
            .map(str::trim_start)
            .find(|l| l.split_whitespace().next() == Some(c))
            .and_then(|l| l.split("  ").next())
            .unwrap_or(c);
        Error::new(
            Code::Usage,
            format!(
                "usage: barocss-hub {}  (see barocss-hub --help)",
                syn.trim_end()
            ),
        )
    };
    if matches!(a.as_slice(), [] | ["help"] | ["--help"] | ["-h"]) {
        if json {
            return json::emit(out, json::doc("help", vec![("text", json::J::s(HELP))]));
        }
        out.write_all(HELP.as_bytes())?;
        return Ok(());
    }
    if matches!(a.as_slice(), ["--version"] | ["-V"]) {
        if json {
            let v = json::J::s(env!("CARGO_PKG_VERSION"));
            return json::emit(out, json::doc("version", vec![("version", v)]));
        }
        out.write_all(VERSION.as_bytes())?;
        return Ok(());
    }
    if !COMMANDS.contains(&a[0]) {
        return errc(
            Code::Usage,
            format!("unknown command '{}' (see barocss-hub --help)", a[0]),
        );
    }
    if json && !json::STRUCTURED.contains(&a[0]) {
        // Commands without a structured form: their human lines, wrapped.
        return json::captured(&args, cwd, out, warn);
    }
    if a[0] == "help" {
        return Err(usage("help"));
    }
    if a[0] == "init" {
        let (root, bare) = match a.as_slice() {
            ["init"] => (cwd.to_path_buf(), false),
            ["init", "--bare"] => (cwd.to_path_buf(), true),
            ["init", "--bare", d] => (cwd.join(d), true),
            _ => return Err(usage("init")),
        };
        if bare {
            fs::create_dir_all(&root)?;
        }
        let r = Repo::init_opts(&root, bare)?;
        writeln!(
            out,
            "initialized {}hub repository in {} (replica {:032x})",
            if bare { "bare " } else { "" },
            r.dir.display(),
            r.replica_id.0
        )?;
        return Ok(());
    }
    if a[0] == "peer" {
        return errc(
            Code::Usage,
            "`peer` was replaced by `remote` (D-041): use `barocss-hub remote add <name> file:///abs/path`",
        );
    }
    if a[0] == "remote" {
        return remote::remote_cmd(cwd, &a[1..], out);
    }
    if a[0] == "serve" {
        return remote::serve_cmd(cwd, &a[1..]);
    }
    if matches!(
        a[0],
        "record" | "checkout" | "mkdir" | "touch" | "mv" | "rm" | "watch"
    ) {
        if let Ok(hub) = sync::find_hub(cwd) {
            remote::refuse_bare(&hub, a[0])?;
        }
    }
    if a[0] == "watch" {
        let mut opts = watch::parse_opts(&a[1..])?;
        opts.json = json;
        return watch::watch_cmd(cwd, opts, out, warn);
    }
    if a[0] == "sync" {
        let yes = a.iter().any(|x| matches!(*x, "--yes" | "-y"));
        let rest: Vec<&str> = a[1..]
            .iter()
            .copied()
            .filter(|x| !matches!(*x, "--yes" | "-y"))
            .collect();
        let arg = match rest.as_slice() {
            [] => None,
            [p] => Some(*p),
            _ => return Err(usage("sync")),
        };
        if json {
            let mode = remote::cli_trust_mode(yes, true);
            let r = remote::sync(cwd, arg, true, mode, warn)?;
            return json::emit(out, json::sync_doc(&r, warn)?);
        }
        return remote::sync_cmd(cwd, arg, yes, out, warn);
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
            let (kind, verb) = if a[0] == "mkdir" {
                (Kind::Directory, "mkdir")
            } else {
                (Kind::File, "touch")
            };
            mutate(
                &mut repo,
                verb,
                Payload::Create { kind, parent, name },
                warn,
            )?;
        }
        ["mv", from, to] => {
            let node = lookup(&proj, from)?;
            if node == hub_core::tree::ROOT {
                return errc(Code::InvalidPath, "cannot move the root");
            }
            let (new_parent, new_name) = resolve_new(&state, &proj, to)?;
            if is_ancestor_or_self(&state, node, new_parent) {
                return errc(
                    Code::InvalidPath,
                    format!("cannot move '{from}' into itself"),
                );
            }
            mutate(
                &mut repo,
                "mv",
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
                return errc(Code::InvalidPath, "cannot remove the root");
            }
            mutate(&mut repo, "rm", Payload::Delete { node }, warn)?;
        }
        ["record"] | ["record", "-m", _] => {
            let msg = a.get(2).copied();
            let mut sink = Vec::new();
            let o: &mut dyn Write = if json { &mut sink } else { out };
            let r = record(&mut repo, msg, false, o, warn)?;
            if json {
                json::emit(out, json::record_doc(&repo, r.as_ref()))?;
            }
        }
        ["checkout", rest @ ..] => {
            let force = rest.contains(&"--force");
            let ids: Vec<&str> = rest.iter().copied().filter(|x| *x != "--force").collect();
            let mut sink = Vec::new();
            let o: &mut dyn Write = if json { &mut sink } else { out };
            let c = match ids.as_slice() {
                [] => checkout(&mut repo, None, force, o, warn)?,
                [id] if !id.starts_with('-') => checkout(&mut repo, Some(id), force, o, warn)?,
                _ => return Err(usage("checkout")),
            };
            if json {
                json::emit(out, json::checkout_doc(&c))?;
            }
        }
        ["cat", id, p] => {
            let (v, node, blob) = json::cat_target(&repo, id, p)?;
            let bytes = repo.blobs()?.get(&blob).map_err(blob_error)?;
            if json {
                json::emit(
                    out,
                    json::cat_doc(&v, &repo.index().short(&v), p, node, blob, &bytes),
                )?;
            } else {
                out.write_all(&bytes)?;
            }
        }
        ["cat-blob", b] => {
            let blob = json::parse_blob(b)?;
            let bytes = repo.blobs()?.get(&blob).map_err(|e| match e {
                blobs::BlobError::Missing(_) => {
                    Error::new(Code::NoSuchBlob, format!("no such blob: {b}"))
                }
                e => blob_error(e),
            })?;
            if json {
                json::emit(out, json::blob_doc(blob, &bytes))?;
            } else {
                out.write_all(&bytes)?;
            }
        }
        ["ls", ..] | ["show", ..] | ["status", ..] | ["log", ..] | ["warnings"] if json => {
            json::read_cmd(&repo, &state, &proj, &a, out, warn)?;
        }
        ["log", "--path", p] => {
            for l in json::path_history_lines(&repo, p)? {
                writeln!(out, "{l}")?;
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
        ["status"] | ["status", "--ignored"] => {
            let ix = repo.index();
            let hid = VersionId::of_frontier(&repo.head_frontier());
            writeln!(
                out,
                "head {} ({} ops, {} checkpoints, {} published)",
                ix.short(&hid),
                repo.store.len(),
                repo.checkpoints.len(),
                sync::read_published(&repo)?.len()
            )?;
            let cs = render_conflicts(&proj, &repo.conflicts());
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
            let remotes = remote::remotes_view(&repo.dir)?;
            if !remotes.is_empty() {
                writeln!(out, "remotes:")?;
                for r in &remotes {
                    writeln!(out, "  {} {}", r.name, r.url)?;
                }
            }
            if remote::is_bare(&repo.dir) {
                // D-042: no working directory, index, pin or watch.
                writeln!(out, "bare repository (no working directory)")?;
                for l in render_tree(&state, &proj, hub_core::tree::ROOT) {
                    writeln!(out, "{l}")?;
                }
                return Ok(());
            }
            if let Some(p) = watch::pinned(&repo.dir)? {
                writeln!(
                    out,
                    "pinned at {} (`barocss-hub checkout <id>` of an older version); watch pauses auto-record and auto-checkout; run `barocss-hub checkout` to resume",
                    ix.short(&VersionId::of_frontier(&p))
                )?;
            }
            if let Some(pid) = watch::active_watch(&repo.dir)? {
                writeln!(out, "watch: active (pid {pid})")?;
            }
            if wd::interrupted(&repo.dir)?.is_some() {
                writeln!(
                    out,
                    "interrupted checkout: run `barocss-hub checkout` to finish it"
                )?;
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
                        "working directory is behind head by {n} op(s); `barocss-hub checkout` brings it up to date"
                    )?;
                }
            }
            let mut ws = Vec::new();
            let (mut w, ignored) = wd::scan_all(&repo.root(), &idx, &mut ws)?;
            w.retain(|p, _| !ignored.contains(p));
            print_warnings(warn, &ws)?;
            let changes = wd::diff(&idx, &w);
            let tbi = wd::tracked_but_ignored(&repo.root(), &idx)?;
            if !tbi.is_empty() {
                writeln!(
                    out,
                    "tracked but ignored (still tracked; `barocss-hub rm` to stop tracking):"
                )?;
                for p in &tbi {
                    writeln!(out, "  {p}")?;
                }
            }
            if a.len() == 1 {
                let n = ignored
                    .iter()
                    .filter(|p| !p.match_indices('/').any(|(i, _)| ignored.contains(&p[..i])))
                    .filter(|p| {
                        ignore::Ignore::is_default_ignored(
                            &repo.root(),
                            p,
                            w_all_is_dir(&repo.root(), p),
                        )
                    })
                    .count();
                if n > 0 {
                    writeln!(
                        out,
                        "{n} file(s) hidden by default ignore rules (see `barocss-hub status --ignored`)"
                    )?;
                }
            }
            if a.len() == 2 && ignored.is_empty() {
                writeln!(out, "ignored: none")?;
            } else if a.len() == 2 {
                writeln!(out, "ignored:")?;
                // Only the top-most ignored path of an ignored subtree.
                for p in &ignored {
                    let covered = p.match_indices('/').any(|(i, _)| ignored.contains(&p[..i]));
                    if !covered {
                        let dir = w_all_is_dir(&repo.root(), p);
                        writeln!(out, "  {p}{}", if dir { "/" } else { "" })?;
                    }
                }
            }
            let unmat: Vec<&String> = idx
                .entries
                .iter()
                .filter(|(_, (_, e))| *e == Entry::Unmaterialized)
                .map(|(p, _)| p)
                .collect();
            if !unmat.is_empty() {
                writeln!(
                    out,
                    "unmaterialized (case clash on a case-insensitive filesystem, D-019):"
                )?;
                for p in unmat {
                    writeln!(out, "  {p}")?;
                }
            }
            let blocked = wd::blocked(&idx, &w);
            if !blocked.is_empty() {
                writeln!(out, "blocked (record refuses until resolved):")?;
                for l in wd::render_blocked(&blocked) {
                    writeln!(out, "  {l}")?;
                }
            }
            if changes.is_empty() && blocked.is_empty() {
                writeln!(out, "working directory: clean")?;
            } else if !changes.is_empty() {
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
            if json {
                json::emit(out, json::commit_doc(&repo, &ix, &c))?;
            } else {
                writeln!(out, "[{}] {}", ix.short(&c.id()), c.message)?;
            }
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
                [] => {
                    return errc(
                        Code::NotFound,
                        format!("no local checkpoint matches '{what}'"),
                    )
                }
                [c] if published.contains(c) => {
                    writeln!(
                        out,
                        "already published: [{}] {}",
                        ix.short(&c.id()),
                        c.message
                    )?;
                }
                [c] if c.message.len() > wire::MAX_MESSAGE_BYTES
                    || c.note
                        .as_ref()
                        .is_some_and(|n| n.len() > wire::MAX_MESSAGE_BYTES) =>
                {
                    return errc(
                        Code::Usage,
                        format!(
                            "checkpoint message exceeds the {}-byte limit; it cannot be published",
                            wire::MAX_MESSAGE_BYTES
                        ),
                    )
                }
                [c] => {
                    sync::append_published(&repo, c)?;
                    writeln!(out, "published [{}] {}", ix.short(&c.id()), c.message)?;
                }
                many => {
                    return errc(
                        Code::Ambiguous,
                        format!(
                            "ambiguous checkpoint '{what}': {}; publish by exact message",
                            many.iter()
                                .map(|c| format!("[{}] {}", ix.short(&c.id()), c.message))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    )
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
            // Issue #32: head is shown from the replayed state (no
            // down-closure, no evaluate); indexed frontiers are canonical,
            // so the resolved key is the version id.
            let (vid, f) = match ix.resolve(id) {
                Ok((vid, f)) if !oracle() && f == repo.head_frontier().as_slice() => (vid, None),
                _ => {
                    let v = resolve(&repo, &ix, id)?;
                    (hub_core::checkpoint::version_id(&repo.store, &v), Some(v))
                }
            };
            let st = match &f {
                None => repo.state().clone(),
                Some(v) => evaluate(v.resolve(&repo.store)).state,
            };
            let p = project(&st);
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
                render_long(&st, &p)
            } else {
                render_tree(&st, &p, hub_core::tree::ROOT)
            };
            for l in lines {
                writeln!(out, "{l}")?;
            }
        }
        _ => return Err(usage(a[0])),
    }
    // Issue #33: writers refresh `.hub/snapshot` when the policy says so.
    if let Some(w) = repo.maybe_snapshot() {
        writeln!(warn, "warning: {w}")?;
    }
    if fast_exit() {
        std::mem::forget(repo);
    }
    Ok(())
}

/// Program name used as the error prefix (`barocss-hub: <message>`) and in
/// `--json` error documents.
pub const PROGRAM: &str = "barocss-hub";

/// The whole process behaviour of `barocss-hub` (Issue #37): run `args`,
/// print errors, return the exit status (see [`json`] for the exit codes).
///
/// Human mode: output and warnings stream to `out`/`err`; an error prints
/// `barocss-hub: <message>` on `err`. `--json` mode: `out` gets exactly one
/// JSON document (warnings folded in as `"warnings"`), or, on an error,
/// nothing on `out` and one JSON error document on `err`. `watch --json`
/// streams events on `out` as it runs.
pub fn main_with(args: &[String], cwd: &Path, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    if !args.iter().any(|x| x == json::FLAG) {
        return match run(args, cwd, out, err) {
            Ok(()) => 0,
            Err(e) if e.is_broken_pipe() => 0,
            Err(e) => {
                let _ = writeln!(err, "{PROGRAM}: {e}");
                json::EXIT_ERROR
            }
        };
    }
    let streaming = args.iter().find(|x| *x != json::FLAG).map(String::as_str) == Some("watch");
    let (mut doc, mut wbuf) = (Vec::new(), Vec::new());
    let r = if streaming {
        run(args, cwd, out, &mut wbuf)
    } else {
        run(args, cwd, &mut doc, &mut wbuf)
    };
    let warnings = json::warning_lines(&wbuf);
    match r {
        Ok(()) => {
            if !streaming {
                let _ = out.write_all(&json::with_warnings(&doc, &warnings));
            }
            0
        }
        Err(e) if e.is_broken_pipe() => 0,
        Err(e) => {
            let _ = json::emit(err, json::error_doc(&e, &warnings));
            json::EXIT_ERROR
        }
    }
}

/// Issue #31: the lazy checkpoint decode gives the same `hub` output as the
/// pre-#31 eager oracle (`store::decode_ckpt_eager`, switched on with
/// `store::EAGER`): open errors for invalid stored frontiers, `log`,
/// `status`, `show` and resolution errors, over random multi-replica
/// histories with random local and published checkpoints.
#[cfg(test)]
mod lazy_ckpt_tests {
    use super::*;
    use proptest::prelude::*;

    fn run_all(root: &Path, ids: &[String], eager: bool) -> Vec<Result<String, String>> {
        store::EAGER.with(|e| e.set(eager));
        let mut cmds: Vec<Vec<String>> = vec![vec!["log".into()], vec!["status".into()]];
        for id in ids {
            cmds.push(vec!["show".into(), id.clone()]);
        }
        let out = cmds
            .iter()
            .map(|c| {
                let (mut o, mut w) = (Vec::new(), Vec::new());
                run(c, root, &mut o, &mut w)
                    .map(|()| {
                        String::from_utf8(o).unwrap() + "\n--\n" + &String::from_utf8(w).unwrap()
                    })
                    .map_err(|e| e.0)
            })
            .collect();
        store::EAGER.with(|e| e.set(false));
        out
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: crate::test_cases(128), ..ProptestConfig::default() })]

        #[test]
        fn cli_lazy_decode_equals_eager(
            nops in 1usize..24,
            seed in any::<u64>(),
            nck in 0usize..6,
        ) {
            let mut s = seed | 1;
            let mut rnd = move |n: usize| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s % n.max(1) as u64) as usize
            };
            let root = std::env::temp_dir().join(format!(
                "hub-lazyck-{}-{seed:x}-{nops}-{nck}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            let mut repo = Repo::init(&root).unwrap();
            // Lamport, closed, multi-replica history: counter = index + 2.
            let mut ids: Vec<NodeId> = Vec::new();
            for i in 0..nops {
                let mut deps = BTreeSet::new();
                for _ in 0..rnd(3) {
                    if !ids.is_empty() {
                        deps.insert(ids[rnd(ids.len())]);
                    }
                }
                let id = NodeId::new(i as u64 + 2, 1 + rnd(3) as u128);
                repo.append_op(Op {
                    id,
                    deps,
                    payload: Payload::Create {
                        kind: Kind::Directory,
                        parent: hub_core::tree::ROOT,
                        name: format!("d{i}"),
                    },
                })
                .unwrap();
                ids.push(id);
            }
            let unknown = rnd(4) == 0;
            let mut shows = vec!["zzzz".to_string(), "0000".to_string()];
            let mut published = Vec::new();
            for k in 0..nck {
                // Often non-antichain (comparable ids), sometimes empty.
                let mut f: Vec<NodeId> = (0..rnd(4)).map(|_| ids[rnd(ids.len())]).collect();
                if unknown && k == nck - 1 {
                    f.push(NodeId::new(999, 0xbad));
                }
                f.sort();
                f.dedup();
                let body = format!(
                    "K {:032x} {} {} -",
                    rnd(3) + 1,
                    if f.is_empty() {
                        "-".to_string()
                    } else {
                        f.iter().map(|i| store::fmt_id(*i)).collect::<Vec<_>>().join(",")
                    },
                    store::esc(&format!("m{k}"))
                );
                store::append_record(&repo.dir.join("checkpoints.log"), &body).unwrap();
                if let Ok(c) = store::decode_ckpt_eager(&body, &repo.store) {
                    shows.push(c.id().hex()[..8].to_string());
                    shows.push(c.id().hex()[..4].to_string());
                }
                if rnd(2) == 0 && !(unknown && k == nck - 1) {
                    published.push(body);
                }
            }
            if !published.is_empty() {
                let p = repo.dir.join("published.log");
                store::write_atomic(&p, format!("{}\n", sync::PUBLISHED_HEADER).as_bytes()).unwrap();
                for b in &published {
                    store::append_record(&p, b).unwrap();
                }
            }
            drop(repo);
            let lazy = run_all(&root, &shows, false);
            let eager = run_all(&root, &shows, true);
            let _ = fs::remove_dir_all(&root);
            prop_assert_eq!(&lazy, &eager);
            if unknown && nck > 0 {
                prop_assert!(lazy[0].as_ref().is_err_and(|e| e.contains("checkpoint frontier not in op log")), "{:?}", lazy[0]);
            }
        }
    }
}

/// Issue #32: every command's output (stdout, stderr, result) and the
/// resulting logs and working tree are byte-identical between the new paths
/// and the pre-#32 oracle paths ([`ORACLE`]), over random repositories: two
/// copies of one repo (same replica id) receive the same working-directory
/// edits, commands and foreign concurrent ops (moves, deletes, content
/// edits, so conflicts of several kinds and a head the working directory is
/// behind), one copy run with the oracle on.
#[cfg(test)]
mod perf32_tests {
    use super::*;
    use proptest::prelude::*;

    fn copy_dir(a: &Path, b: &Path) {
        fs::create_dir_all(b).unwrap();
        for e in fs::read_dir(a).unwrap() {
            let e = e.unwrap();
            let t = b.join(e.file_name());
            if e.file_type().unwrap().is_dir() {
                copy_dir(&e.path(), &t);
            } else {
                fs::copy(e.path(), &t).unwrap();
            }
        }
    }

    /// Working tree (minus `.hub/`) plus the logs, as comparable text.
    fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn walk(base: &Path, d: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
            let mut es: Vec<_> = fs::read_dir(d).unwrap().map(|e| e.unwrap()).collect();
            es.sort_by_key(|e| e.file_name());
            for e in es {
                let p = e.path();
                let rel = p.strip_prefix(base).unwrap().to_string_lossy().to_string();
                if rel == HUB_DIR {
                    continue;
                }
                if e.file_type().unwrap().is_dir() {
                    out.insert(format!("{rel}/"), Vec::new());
                    walk(base, &p, out);
                } else {
                    out.insert(rel, fs::read(&p).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        for f in ["ops.log", "checkpoints.log", "published.log", "pinned"] {
            if let Ok(b) = fs::read(root.join(HUB_DIR).join(f)) {
                out.insert(format!(".hub/{f}"), b);
            }
        }
        out
    }

    /// Snapshot mode of one copy (Issue #33): never, or after every write.
    #[derive(Clone, Copy)]
    enum Snap {
        Off,
        Always,
    }

    fn with_snap<T>(m: Snap, f: impl FnOnce() -> T) -> T {
        match m {
            Snap::Off => snapshot::TEST_OFF.with(|c| c.set(true)),
            Snap::Always => snapshot::TEST_MIN.with(|c| c.set(Some(0))),
        }
        let r = f();
        snapshot::TEST_OFF.with(|c| c.set(false));
        snapshot::TEST_MIN.with(|c| c.set(None));
        r
    }

    fn run1(
        root: &Path,
        args: &[String],
        oracle_on: bool,
        snap: Snap,
    ) -> (Result<(), String>, String, String) {
        ORACLE.with(|o| o.set(oracle_on));
        let (mut o, mut w) = (Vec::new(), Vec::new());
        let r = with_snap(snap, || run(args, root, &mut o, &mut w).map_err(|e| e.0));
        ORACLE.with(|o| o.set(false));
        (
            r,
            String::from_utf8(o).unwrap(),
            String::from_utf8(w).unwrap(),
        )
    }

    /// Run `args` in every copy (new in `a`, oracle in `b`, both without
    /// snapshots; new with a snapshot after every write in `c`, #33) and
    /// compare stdout, stderr, results, working trees and logs.
    fn both(a: &Path, b: &Path, args: &[&str]) -> Result<String, TestCaseError> {
        let c = a.parent().unwrap().join("c");
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let x = run1(a, &args, false, Snap::Off);
        let y = run1(b, &args, true, Snap::Off);
        let z = run1(&c, &args, false, Snap::Always);
        prop_assert_eq!(&x, &y, "hub {:?}", args);
        prop_assert_eq!(&x, &z, "hub {:?} (with snapshot)", args);
        prop_assert_eq!(snapshot(a), snapshot(b), "after hub {:?}", args);
        prop_assert_eq!(
            snapshot(a),
            snapshot(&c),
            "after hub {:?} (with snapshot)",
            args
        );
        prop_assert!(!a.join(".hub/snapshot").exists());
        Ok(x.1)
    }

    /// Rewrite `d` as the v0 layout (#33 migration input).
    fn to_v0(d: &Path) {
        let h = d.join(HUB_DIR);
        let log = fs::read_to_string(h.join("ops.log")).unwrap();
        let mut out = format!("{}\n", store::OPLOG_HEADER_V0);
        for l in log.lines().skip(1).filter(|l| l.starts_with("O ")) {
            out.push_str(l);
            out.push('\n');
        }
        fs::write(h.join("ops.log"), out).unwrap();
        let r = fs::read_to_string(h.join("replica")).unwrap();
        fs::write(
            h.join("replica"),
            r.replace(store::REPLICA_HEADER, store::REPLICA_HEADER_V0),
        )
        .unwrap();
        for f in ["snapshot", "format", "watch.lock", "peers"] {
            let _ = fs::remove_file(h.join(f));
        }
    }

    /// A v0 copy of `a`, migrated by its first command, prints the same.
    fn migrated_same(a: &Path, views: &[Vec<String>]) -> Result<(), TestCaseError> {
        let m = a.parent().unwrap().join("m");
        let _ = fs::remove_dir_all(&m);
        copy_dir(a, &m);
        to_v0(&m);
        for (k, v) in views.iter().enumerate() {
            let x = run1(a, v, false, Snap::Off);
            let y = run1(
                &m,
                v,
                false,
                if k % 2 == 0 { Snap::Always } else { Snap::Off },
            );
            let strip = |e: &str| {
                e.lines()
                    .filter(|l| !l.contains("to on-disk format v1"))
                    .map(|l| format!("{l}\n"))
                    .collect::<String>()
            };
            prop_assert_eq!(&x.0, &y.0, "migrated hub {:?}", v);
            prop_assert_eq!(&x.1, &y.1, "migrated hub {:?}", v);
            prop_assert_eq!(strip(&x.2), strip(&y.2), "migrated hub {:?}", v);
            if k == 0 {
                prop_assert!(y.2.contains("to on-disk format v1"));
            }
        }
        let ops = |d: &Path| -> Vec<String> {
            fs::read_to_string(d.join(".hub/ops.log"))
                .unwrap()
                .lines()
                .filter(|l| l.starts_with("O "))
                .map(str::to_string)
                .collect()
        };
        prop_assert_eq!(ops(a), ops(&m));
        let _ = fs::remove_dir_all(&m);
        Ok(())
    }

    /// The same foreign op appended to both copies.
    fn foreign(a: &Path, b: &Path, rnd: &mut dyn FnMut(usize) -> usize, k: usize) {
        let r = Repo::open_dir(a.join(HUB_DIR)).unwrap();
        let st = r.state().clone();
        let nodes: Vec<NodeId> = project(&st)
            .entries
            .values()
            .copied()
            .filter(|n| *n != ROOT)
            .collect();
        let files: Vec<NodeId> = nodes
            .iter()
            .copied()
            .filter(|n| st[n].kind == Kind::File)
            .collect();
        let ids: Vec<NodeId> = r.store.keys().copied().collect();
        let counter = 1 + ids.iter().map(|i| i.counter).max().unwrap_or(1);
        let mut deps = BTreeSet::new();
        if !ids.is_empty() && rnd(3) != 0 {
            deps.insert(ids[rnd(ids.len())]);
        }
        let payload = match rnd(4) {
            0 if !files.is_empty() => {
                let bytes = format!("foreign {k}\n");
                for d in [a, b, &a.parent().unwrap().join("c")] {
                    blobs::Blobs::open(&d.join(HUB_DIR))
                        .unwrap()
                        .put(bytes.as_bytes())
                        .unwrap();
                }
                Payload::SetContent {
                    node: files[rnd(files.len())],
                    blob: BlobId::of(bytes.as_bytes()),
                }
            }
            1 if !nodes.is_empty() => Payload::Move {
                node: nodes[rnd(nodes.len())],
                new_parent: ROOT,
                new_name: format!("mv{}", rnd(3)),
            },
            2 if !nodes.is_empty() => Payload::Delete {
                node: nodes[rnd(nodes.len())],
            },
            _ => Payload::Create {
                kind: Kind::Directory,
                parent: ROOT,
                name: format!("fd{}", rnd(3)),
            },
        };
        // Foreign ops must depend on the nodes they touch (causal delivery).
        if let Payload::SetContent { node, .. }
        | Payload::Move { node, .. }
        | Payload::Delete { node } = &payload
        {
            if r.store.contains_key(node) {
                deps.insert(*node);
            }
        }
        drop(r);
        let op = Op {
            id: NodeId::new(counter, 0xf00d),
            deps,
            payload,
        };
        let c = a.parent().unwrap().join("c");
        for (d, m) in [(a, Snap::Off), (b, Snap::Off), (&c, Snap::Always)] {
            with_snap(m, || {
                let mut r = Repo::open_dir(d.join(HUB_DIR)).unwrap();
                r.append_op(op.clone()).unwrap();
                r.maybe_snapshot();
            });
        }
    }

    fn short_ids(log: &str, status: &str) -> Vec<String> {
        let mut v: Vec<String> = log
            .lines()
            .filter_map(|l| l.split(' ').next().map(str::to_string))
            .collect();
        if let Some(h) = status.lines().next().and_then(|l| l.split(' ').nth(1)) {
            v.push(h.to_string());
        }
        v.push("zzzz".into());
        v.push("0000".into());
        v
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: crate::test_cases(128), ..ProptestConfig::default() })]

        #[test]
        /// Issue #33: also with a snapshot after every write (copy `c`) and
        /// after a v0 -> v1 migration.
        fn cli_output_equals_pre32_oracle(seed in any::<u64>(), nsteps in 1usize..8) {
            let mut s = seed | 1;
            let mut rnd = move |n: usize| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s % n.max(1) as u64) as usize
            };
            let base = std::env::temp_dir().join(format!("hub-p32-{}-{seed:x}-{nsteps}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            let (a, b) = (base.join("a"), base.join("b"));
            fs::create_dir_all(&a).unwrap();
            drop(Repo::init(&a).unwrap());
            copy_dir(&a, &b);
            copy_dir(&a, &base.join("c"));
            for step in 0..nsteps {
                match rnd(8) {
                    0..=2 => {
                        // Working-directory edits in `a`, mirrored onto
                        // `b`, then record (-m sometimes).
                        fs::create_dir_all(a.join("d")).unwrap();
                        for _ in 0..1 + rnd(3) {
                            let dir = if rnd(3) == 0 { "d/" } else { "" };
                            let p = a.join(format!("{dir}f{}", rnd(4)));
                            if rnd(5) == 0 {
                                let _ = fs::remove_file(&p);
                            } else {
                                fs::write(&p, format!("v{}\n", rnd(5))).unwrap();
                            }
                        }
                        for b in [&b, &base.join("c")] {
                        for e in fs::read_dir(b).unwrap() {
                            let e = e.unwrap();
                            if e.file_name() == HUB_DIR {
                                continue;
                            }
                            if e.file_type().unwrap().is_dir() {
                                fs::remove_dir_all(e.path()).unwrap();
                            } else {
                                fs::remove_file(e.path()).unwrap();
                            }
                        }
                        for e in fs::read_dir(&a).unwrap() {
                            let e = e.unwrap();
                            if e.file_name() == HUB_DIR {
                                continue;
                            }
                            let t = b.join(e.file_name());
                            if e.file_type().unwrap().is_dir() {
                                copy_dir(&e.path(), &t);
                            } else {
                                fs::copy(e.path(), &t).unwrap();
                            }
                        }
                        }
                        if rnd(2) == 0 {
                            both(&a, &b, &["record", "-m", &format!("m{step}")])?;
                        } else {
                            both(&a, &b, &["record"])?;
                        }
                    }
                    3 => foreign(&a, &b, &mut rnd, step),
                    4 => {
                        let log = both(&a, &b, &["log"])?;
                        let st = both(&a, &b, &["status"])?;
                        let ids = short_ids(&log, &st);
                        let id = ids[rnd(ids.len())].clone();
                        both(&a, &b, &["checkout", &id])?;
                    }
                    5 => {
                        both(&a, &b, &["checkout"])?;
                    }
                    6 => {
                        let p = format!("t{}", rnd(3));
                        let cmd = ["mkdir", "touch", "rm", "mv"][rnd(4)];
                        if cmd == "mv" {
                            both(&a, &b, &["mv", &p, &format!("t{}", rnd(3) + 3)])?;
                        } else {
                            both(&a, &b, &[cmd, &p])?;
                        }
                    }
                    _ => {
                        both(&a, &b, &["commit", "-m", &format!("c{step}")])?;
                        if rnd(2) == 0 {
                            both(&a, &b, &["publish", &format!("c{step}")])?;
                        }
                    }
                }
                let log = both(&a, &b, &["log"])?;
                let st = both(&a, &b, &["status"])?;
                both(&a, &b, &["status", "--ignored"])?;
                both(&a, &b, &["ls"])?;
                both(&a, &b, &["ls", "-l"])?;
                both(&a, &b, &["warnings"])?;
                for id in short_ids(&log, &st) {
                    both(&a, &b, &["show", &id])?;
                    both(&a, &b, &["show", "-l", &id])?;
                }
                if step + 1 == nsteps || rnd(4) == 0 {
                    let mut vs: Vec<Vec<String>> = [
                        &["status"][..], &["log"], &["ls", "-l"], &["warnings"], &["status", "--ignored"],
                    ]
                    .iter()
                    .map(|v| v.iter().map(|s| s.to_string()).collect())
                    .collect();
                    for id in short_ids(&log, &st) {
                        vs.push(vec!["show".into(), "-l".into(), id]);
                    }
                    migrated_same(&a, &vs)?;
                }
            }
            let _ = fs::remove_dir_all(&base);
        }
    }
}
