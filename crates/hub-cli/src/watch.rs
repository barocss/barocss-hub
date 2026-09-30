//! `hub peer` and `hub watch` (Issue #25): automatic record plus continuous
//! sync with local peers.
//!
//! # Peers
//! `.hub/peers` (local config, never replicated), format v0
//! [decision-proposal]: a header line `barocss-hub/peers/v0`, then one
//! canonical absolute peer root per line.
//!
//! # Change detection: polling [decision-proposal]
//! The watcher polls a cheap *fingerprint* of the working directory (path,
//! kind, size, mtime of every non-ignored entry outside `.hub/`) every
//! `--poll` ms, plus the length of `.hub/ops.log` (grows when a peer's
//! watcher or a manual `hub sync` delivers ops). OS notifications (the
//! `notify` crate) were considered and rejected for v0: they add ~10
//! transitive crates (per-platform backends: fsevent-sys/kqueue/inotify,
//! crossbeam-channel, walkdir, ...), still need a polling fallback for
//! network filesystems and overflowed event queues, and would still require
//! the same scan before recording. A metadata walk is O(entries) and a few ms
//! for source-sized trees; replacing it with notifications later changes
//! only [`fingerprint`]'s trigger, not the record/apply path.
//!
//! # Debounce
//! A local change is acted on only after the fingerprint has been stable
//! for `--debounce` ms (default 400), so a burst of saves becomes one
//! record and a half-written file is not captured mid-write. Incoming ops
//! are also applied only when the working directory is quiet: applying
//! first records the local state, which must not be half-written either.
//!
//! # One step
//! Each step opens the repo under the exclusive lock (so it is atomic with
//! respect to other hub commands) and runs, in order:
//! 1. resume an interrupted checkout (`.hub/checkout-in-progress`), exactly
//!    as `hub checkout` would (never clobbering unrecorded edits);
//! 2. `record` with ops depending on the index base (`allow_behind`), which
//!    also materializes the merged head over the recorded working tree;
//!    blocked paths and the #24 ignore rules apply unchanged;
//! 3. if nothing was recorded and the working directory is behind head,
//!    `checkout` of head without `--force` (refuses rather than clobbers).
//!
//! Then, when a record happened or the `--sync` interval elapsed, it syncs
//! with every peer through the #22 path (both locks in canonical order) and,
//! if that brought ops, runs one more step to apply them. No checkpoint is
//! made by auto-record [decision-proposal]: only ops, the user checkpoints.
//!
//! # No feedback loop (self-write suppression)
//! After every step the fingerprint and `ops.log` length are re-read and
//! adopted as the new baseline, so the watcher's own writes (checkout,
//! record's rename materialization, `.hub/` files) never look like user
//! edits. A user edit that lands *during* a step would be absorbed into
//! that baseline; the periodic `--sync` tick therefore also runs a record
//! step, which catches it (a no-change record appends nothing). Even if a
//! self-write were noticed, `record` would find nothing to record, so the op
//! count cannot grow without a real change.
//!
//! # Pinned (D-020, repair)
//! `hub checkout <id>` of a non-head version writes `.hub/pinned`
//! ([`set_pinned`]). While it exists the step does nothing but announce
//! "paused" once: no auto-record (which would record on an old base, which
//! plain `record` refuses) and no auto-checkout (which would undo the
//! user's checkout). Syncing continues, so the log keeps up. Plain
//! `hub checkout` (head) removes the pin and the next step resumes.
//!
//! # Signals and crashes
//! SIGINT/SIGTERM set a flag; the in-flight step (which holds the repo
//! lock) runs to completion, then the watcher removes `.hub/watch.pid`,
//! releases its locks and exits 0. After SIGKILL the kernel drops every
//! `flock`; the next command or watch start recovers through the existing
//! paths: the checkout marker is resumed (step 1), a torn log tail is
//! dropped on open, temps are swept by record/checkout.
//!
//! # Watch liveness
//! The watcher holds a non-blocking exclusive `flock` on `.hub/watch.lock`
//! for its lifetime and writes its pid to `.hub/watch.pid`. `hub status`
//! reports a watch active only if that lock is held, so a stale pid file
//! (after SIGKILL) is never mistaken for a live watcher; a second
//! `hub watch` refuses while the lock is held.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::json::J;
use crate::store::{self, LockMode};
use crate::sync::find_hub;
use crate::{err, wd, Error, Repo};

pub const PIN_FILE: &str = "pinned";
pub const PIN_HEADER: &str = "barocss-hub/pinned/v0";

/// `.hub/pinned` (Issue #25 repair, D-020): present iff the user checked
/// out a version other than head (`hub checkout <id>`). Format v0: header
/// line, then the pinned frontier (comma-joined op ids). Written with
/// temp + fsync + rename; removed by a checkout that lands on head
/// (including `hub checkout <id>` naming head itself, which never pins).
/// While present, `hub watch` pauses auto-record and auto-checkout (it still
/// syncs the log), so a deliberate old checkout is neither undone nor
/// recorded on the old base.
pub fn set_pinned(hub: &Path, f: Option<&Vec<hub_core::tree::OpId>>) -> Result<(), Error> {
    let p = hub.join(PIN_FILE);
    match f {
        Some(f) => {
            let ids: Vec<String> = f.iter().map(|i| store::fmt_id(*i)).collect();
            let body = if ids.is_empty() {
                "-".into()
            } else {
                ids.join(",")
            };
            store::write_atomic(&p, format!("{PIN_HEADER}\n{body}\n").as_bytes())?;
        }
        None => match fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        },
    }
    Ok(())
}

/// The pinned frontier, if the working directory is pinned.
pub fn pinned(hub: &Path) -> Result<Option<Vec<hub_core::tree::OpId>>, Error> {
    let p = hub.join(PIN_FILE);
    let text = match fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let lines: Vec<&str> = text.lines().collect();
    match lines.as_slice() {
        [h, "-"] if *h == PIN_HEADER => Ok(Some(Vec::new())),
        [h, ids] if *h == PIN_HEADER => ids
            .split(',')
            .map(|t| store::parse_id(t).map_err(|e| Error(format!("corrupt {}: {e}", p.display()))))
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        _ => err(format!("corrupt {}", p.display())),
    }
}

/// v1 peers header (Issue #33: always present, so v0 binaries refuse).
pub const PEERS_HEADER: &str = "barocss-hub/peers/v1";
/// v0 peers header, still read (a repository mid-migration).
pub const PEERS_HEADER_V0: &str = "barocss-hub/peers/v0";
pub const PID_FILE: &str = "watch.pid";
pub const LOCK_FILE: &str = "watch.lock";

// ---------- peers ----------

pub fn read_peers(hub: &Path) -> Result<Vec<PathBuf>, Error> {
    let p = hub.join("peers");
    let text = match fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut lines = text.lines();
    if !matches!(lines.next(), Some(PEERS_HEADER | PEERS_HEADER_V0)) {
        return err(format!("corrupt {}", p.display()));
    }
    Ok(lines.filter(|l| !l.is_empty()).map(PathBuf::from).collect())
}

pub(crate) fn write_peers(hub: &Path, peers: &[PathBuf]) -> Result<(), Error> {
    let mut s = format!("{PEERS_HEADER}\n");
    for p in peers {
        s.push_str(&p.to_string_lossy());
        s.push('\n');
    }
    store::write_atomic(&hub.join("peers"), s.as_bytes())?;
    Ok(())
}

fn peer_root(cwd: &Path, arg: &str) -> Result<PathBuf, Error> {
    let p = cwd.join(arg);
    if !p.is_dir() {
        return err(format!("no such directory: {arg}"));
    }
    // Canonicalize first: a lexical walk of `a/../b` would find a's `.hub`.
    let root = fs::canonicalize(&p)?;
    if !root.join(crate::HUB_DIR).is_dir() {
        return err(format!(
            "no hub repository at {arg} (run `barocss-hub init` there)"
        ));
    }
    let s = root.to_string_lossy();
    if s.contains('\n') || s.contains('\r') {
        return err("peer path contains a newline");
    }
    Ok(root)
}

/// `hub peer add|list|rm`.
pub fn peer_cmd(cwd: &Path, args: &[&str], out: &mut dyn Write) -> Result<(), Error> {
    let hub = fs::canonicalize(find_hub(cwd)?)?;
    // The repo lock serializes peers-file updates with other writers.
    let _repo_lock = store::RepoLock::acquire(&hub, LockMode::Exclusive)?;
    let mut peers = read_peers(&hub)?;
    match args {
        ["list"] => {
            for p in &peers {
                writeln!(out, "{}", p.display())?;
            }
        }
        ["add", arg] => {
            let root = peer_root(cwd, arg)?;
            if hub.parent() == Some(root.as_path()) {
                return err("cannot add this repository as its own peer");
            }
            if peers.contains(&root) {
                writeln!(out, "already a peer: {}", root.display())?;
            } else {
                peers.push(root.clone());
                write_peers(&hub, &peers)?;
                writeln!(out, "added peer {}", root.display())?;
            }
        }
        ["rm", arg] => {
            let target = match peer_root(cwd, arg) {
                Ok(r) => r,
                // A peer that no longer exists can still be removed by path.
                Err(_) => PathBuf::from(arg),
            };
            let before = peers.len();
            peers.retain(|p| *p != target);
            if peers.len() == before {
                return err(format!("not a peer: {arg}"));
            }
            write_peers(&hub, &peers)?;
            writeln!(out, "removed peer {}", target.display())?;
        }
        _ => {
            return err(
                "usage: barocss-hub peer add <path> | list | rm <path>  (see barocss-hub --help)",
            )
        }
    }
    Ok(())
}

/// `Some(pid)` if a watcher currently holds `.hub/watch.lock`.
pub fn active_watch(hub: &Path) -> Result<Option<String>, Error> {
    if !hub.join(LOCK_FILE).exists() {
        return Ok(None);
    }
    match store::try_lock_exclusive(&hub.join(LOCK_FILE))? {
        Some(_f) => Ok(None),
        None => Ok(Some(
            fs::read_to_string(hub.join(PID_FILE))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "?".into()),
        )),
    }
}

// ---------- signals ----------

static STOP: AtomicBool = AtomicBool::new(false);
/// Whether the current pause was already announced.
static PAUSE_SAID: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
mod sig {
    use std::os::raw::c_int;
    pub const SIGINT: c_int = 2;
    pub const SIGTERM: c_int = 15;
    extern "C" {
        pub fn signal(sig: c_int, handler: usize) -> usize;
    }
    pub extern "C" fn on_signal(_: c_int) {
        super::STOP.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

fn install_signals() {
    #[cfg(unix)]
    // SAFETY: the handler only stores to an atomic (async-signal-safe).
    unsafe {
        let h = sig::on_signal as extern "C" fn(std::os::raw::c_int) as usize;
        sig::signal(sig::SIGINT, h);
        sig::signal(sig::SIGTERM, h);
    }
}

fn stopping() -> bool {
    STOP.load(Ordering::SeqCst)
}

// ---------- fingerprint ----------

/// Hash of (path, kind, size, mtime) of every non-ignored entry under
/// `root`, excluding the top-level `.hub/`. Ignored directories are not
/// descended (tracked-but-ignored paths are covered by the periodic tick).
pub fn fingerprint(root: &Path) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let ig = crate::ignore::Ignore::load(root).unwrap_or_else(|_| crate::ignore::Ignore::parse(""));
    let mut items: Vec<(String, bool, u64, u128)> = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if rel.is_empty() && name == crate::HUB_DIR {
                continue;
            }
            let p = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let Ok(m) = fs::symlink_metadata(e.path()) else {
                continue;
            };
            let is_dir = m.is_dir();
            if ig.is_ignored(&p, is_dir) {
                continue;
            }
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            items.push((p.clone(), is_dir, if is_dir { 0 } else { m.len() }, mtime));
            if is_dir {
                stack.push((e.path(), p));
            }
        }
    }
    items.sort();
    let mut h = DefaultHasher::new();
    items.hash(&mut h);
    h.finish()
}

fn oplog_len(hub: &Path) -> u64 {
    fs::metadata(hub.join("ops.log"))
        .map(|m| m.len())
        .unwrap_or(0)
}

// ---------- the loop ----------

pub struct Opts {
    pub poll: Duration,
    pub debounce: Duration,
    pub sync_every: Duration,
    /// `--json` (Issue #37): JSON Lines events on stdout instead of text.
    pub json: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            poll: Duration::from_millis(100),
            debounce: Duration::from_millis(400),
            sync_every: Duration::from_secs(2),
            json: false,
        }
    }
}

pub fn parse_opts(args: &[&str]) -> Result<Opts, Error> {
    let mut o = Opts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let ms = |v: Option<&&str>| -> Result<Duration, Error> {
            v.and_then(|s| s.parse::<u64>().ok())
                .filter(|n| *n > 0)
                .map(Duration::from_millis)
                .ok_or_else(|| Error(format!("{a} needs a positive number of milliseconds")))
        };
        match *a {
            "--poll" => o.poll = ms(it.next())?,
            "--debounce" => o.debounce = ms(it.next())?,
            "--sync" => o.sync_every = ms(it.next())?,
            _ => return err(
                "usage: barocss-hub watch [--poll <ms>] [--debounce <ms>] [--sync <ms>]  (see barocss-hub --help)",
            ),
        }
    }
    Ok(o)
}

/// What one step did (Issue #37: the source of `watch --json` events; the
/// human text is written by the step itself).
#[derive(Default)]
struct Step {
    resumed_checkout: Option<crate::CheckedOut>,
    paused: Option<hub_core::checkpoint::VersionId>,
    /// The pause was announced by this step (announced once per pause).
    paused_new: bool,
    resumed: bool,
    /// Deferred: an editor save is in progress (see [`save_in_progress`]).
    deferred: Vec<String>,
    recorded: Option<crate::Recorded>,
    checked_out: Option<crate::CheckedOut>,
    head: Option<hub_core::checkpoint::VersionId>,
}

/// One locked step: resume, record (allow behind), else checkout head.
fn step(hub: &Path, out: &mut dyn Write, warn: &mut dyn Write) -> Result<Step, Error> {
    let mut repo = Repo::open_dir_with(hub.to_path_buf(), LockMode::Exclusive)?;
    let mut r = step_in(&mut repo, out, warn);
    if let Ok(s) = &mut r {
        s.head = Some(hub_core::checkpoint::VersionId::of_frontier(
            &repo.head_frontier(),
        ));
    }
    // Issue #33: refresh `.hub/snapshot` when due.
    if let Some(w) = repo.maybe_snapshot() {
        writeln!(warn, "warning: {w}")?;
    }
    r
}

/// Longest time a step defers recording for a save in progress.
fn defer_limit() -> Duration {
    Duration::from_millis(
        std::env::var("HUB_WATCH_SAVE_WAIT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5000),
    )
}

/// When the current deferral started (`None`: not deferring).
static DEFER_SINCE: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

/// Issue #37: tracked files that are missing while the editor's own
/// sidecar of that very file exists: `x~` (vim `backupcopy=no`: x is
/// renamed to x~, then the new x is written), `x___jb_old___` /
/// `x___jb_tmp___` (JetBrains safe write). Recording at that instant
/// would record a delete (and later a create: a new node), so the watcher
/// waits for the save to finish, at most [`defer_limit`] (a lingering
/// backup after a real delete must not block recording forever).
pub fn save_in_progress(root: &Path, idx: &wd::Index) -> Vec<String> {
    let mut v = Vec::new();
    for (p, (_, e)) in &idx.entries {
        if !matches!(e, wd::Entry::File { .. }) || fs::symlink_metadata(root.join(p)).is_ok() {
            continue;
        }
        let side = ["~", "___jb_old___", "___jb_tmp___"]
            .iter()
            .any(|s| fs::symlink_metadata(root.join(format!("{p}{s}"))).is_ok());
        if side {
            v.push(p.clone());
        }
    }
    v
}

fn step_in(repo: &mut Repo, out: &mut dyn Write, warn: &mut dyn Write) -> Result<Step, Error> {
    let mut st = Step::default();
    for w in &repo.warnings {
        writeln!(warn, "warning: {w}")?;
    }
    if wd::interrupted(&repo.dir)?.is_some() {
        st.resumed_checkout = Some(crate::checkout(repo, None, false, out, warn)?);
    }
    if let Some(p) = pinned(&repo.dir)? {
        let vid = hub_core::checkpoint::VersionId::of_frontier(&p);
        let msg = format!(
            "paused: working directory checked out at {} (pinned); run `barocss-hub checkout` to resume watching",
            repo.index().short(&vid)
        );
        if !PAUSE_SAID.swap(true, Ordering::SeqCst) {
            writeln!(out, "{msg}")?;
            st.paused_new = true;
        }
        st.paused = Some(vid);
        return Ok(st);
    }
    if PAUSE_SAID.swap(false, Ordering::SeqCst) {
        writeln!(out, "resumed: working directory back at head")?;
        st.resumed = true;
    }
    {
        let idx = wd::Index::load(&repo.dir)?;
        let busy = save_in_progress(&repo.root(), &idx);
        let mut since = DEFER_SINCE.lock().unwrap_or_else(|e| e.into_inner());
        if busy.is_empty() {
            *since = None;
        } else {
            let t0 = *since.get_or_insert_with(Instant::now);
            if t0.elapsed() < defer_limit() {
                st.deferred = busy;
                return Ok(st);
            }
            *since = None;
        }
    }
    let mut quiet = Vec::new();
    st.recorded = crate::record(repo, None, true, &mut quiet, warn)?;
    if st.recorded.is_some() {
        out.write_all(&quiet)?;
        return Ok(st);
    }
    let idx = wd::Index::load(&repo.dir)?;
    if idx.base != repo.head_frontier() {
        st.checked_out = Some(crate::checkout(repo, None, false, out, warn)?);
    }
    Ok(st)
}

/// Parse a sync summary line's six counts (received ops, blobs,
/// checkpoints; sent ops, blobs, checkpoints).
fn sync_counts(line: &str) -> Option<[u64; 6]> {
    let rest = line.split_once(": received ")?.1;
    let nums: Vec<u64> = rest
        .split(|c: char| !c.is_ascii_digit())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse().ok())
        .collect();
    nums.try_into().ok()
}

/// Output of the watcher: human text or JSON Lines events (Issue #37).
struct Io<'a> {
    json: bool,
    out: &'a mut dyn Write,
    warn: &'a mut dyn Write,
}

impl Io<'_> {
    fn event(&mut self, name: &str, fields: Vec<(&str, J)>) -> Result<(), Error> {
        if self.json {
            crate::json::emit(self.out, crate::json::event(name, fields))?;
        }
        Ok(())
    }
    /// Human: `warning: <msg>` on stderr; JSON: a `warning` event.
    fn warning(&mut self, msg: &str) -> Result<(), Error> {
        if self.json {
            self.event("warning", vec![("message", J::s(msg))])
        } else {
            writeln!(self.warn, "warning: {msg}")?;
            Ok(())
        }
    }
    /// Run `f` with the real streams (human) or with buffers whose
    /// warnings become events and whose text is dropped (JSON).
    fn call<T>(
        &mut self,
        f: impl FnOnce(&mut dyn Write, &mut dyn Write) -> Result<T, Error>,
    ) -> Result<T, Error> {
        if !self.json {
            return f(self.out, self.warn);
        }
        let (mut o, mut w) = (Vec::new(), Vec::new());
        let r = f(&mut o, &mut w);
        for l in crate::json::warning_lines(&w) {
            self.warning(&l)?;
        }
        r
    }
    fn flush(&mut self) -> Result<(), Error> {
        self.out.flush()?;
        self.warn.flush()?;
        Ok(())
    }
}

fn version_j(v: &hub_core::checkpoint::VersionId) -> J {
    J::obj(vec![("id", J::s(v.hex())), ("short", J::s(v.short()))])
}

/// The events of one step.
fn step_events(io: &mut Io, s: &Step) -> Result<(), Error> {
    if let Some(c) = &s.resumed_checkout {
        let mut f = crate::json::checkout_fields(c);
        f.push(("reason", J::s("resume-interrupted")));
        io.event("checked-out", f)?;
    }
    if let (Some(v), true) = (&s.paused, s.paused_new) {
        io.event("paused", vec![("pinned", version_j(v))])?;
    }
    if s.resumed {
        io.event("resumed", vec![])?;
    }
    if !s.deferred.is_empty() {
        io.event("deferred", vec![("paths", J::strs(&s.deferred))])?;
    }
    let head = || J::opt(s.head.as_ref().map(version_j));
    if let Some(r) = &s.recorded {
        io.event(
            "recorded",
            vec![
                ("changes", crate::json::changes(&r.changes)),
                ("head", head()),
            ],
        )?;
    }
    if let Some(c) = &s.checked_out {
        let mut f = crate::json::checkout_fields(c);
        f.push(("reason", J::s("incoming")));
        io.event("checked-out", f)?;
    }
    Ok(())
}

fn sync_all(hub: &Path, io: &mut Io) -> Result<(), Error> {
    let root = hub.parent().unwrap_or(hub).to_path_buf();
    for p in read_peers(hub)? {
        let mut sink = Vec::new();
        let r =
            io.call(|_, warn| crate::sync::sync_cmd(&root, &p.to_string_lossy(), &mut sink, warn));
        if let Err(e) = r {
            if io.json {
                io.event(
                    "error",
                    vec![
                        ("stage", J::s("sync")),
                        ("peer", J::s(p.to_string_lossy())),
                        ("message", J::s(&e.0)),
                    ],
                )?;
            } else {
                io.warning(&format!("sync with {} failed: {e}", p.display()))?;
            }
        } else {
            // Only report syncs that moved something (first line only; the
            // checkout note does not apply, the watcher applies itself).
            let s = String::from_utf8_lossy(&sink);
            let first = s.lines().next().unwrap_or("");
            if !first.contains("received 0 op(s), 0 blob(s), 0 checkpoint(s); sent 0 op(s), 0 blob(s), 0 checkpoint(s)") {
                if io.json {
                    let n = sync_counts(first).unwrap_or([0; 6]);
                    let counts = |i: usize| {
                        J::obj(vec![
                            ("ops", J::Num(n[i])),
                            ("blobs", J::Num(n[i + 1])),
                            ("checkpoints", J::Num(n[i + 2])),
                        ])
                    };
                    io.event(
                        "synced",
                        vec![
                            ("peer", J::s(p.to_string_lossy())),
                            ("received", counts(0)),
                            ("sent", counts(3)),
                        ],
                    )?;
                } else {
                    writeln!(io.out, "{first}")?;
                }
            }
        }
    }
    Ok(())
}

struct PidGuard(PathBuf);

impl Drop for PidGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// `hub watch`: runs until SIGINT/SIGTERM, then exits cleanly.
pub fn watch_cmd(
    cwd: &Path,
    opts: Opts,
    out: &mut dyn Write,
    warn: &mut dyn Write,
) -> Result<(), Error> {
    let hub = fs::canonicalize(find_hub(cwd)?)?;
    let root = hub.parent().unwrap_or(&hub).to_path_buf();
    let Some(_watch_lock) = store::try_lock_exclusive(&hub.join(LOCK_FILE))? else {
        return err(format!(
            "a watch is already running on this repository (pid {})",
            fs::read_to_string(hub.join(PID_FILE))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "?".into())
        ));
    };
    install_signals();
    store::write_atomic(
        &hub.join(PID_FILE),
        format!("{}\n", std::process::id()).as_bytes(),
    )?;
    let _pid = PidGuard(hub.join(PID_FILE));
    let mut io = Io {
        json: opts.json,
        out,
        warn,
    };
    let npeers = read_peers(&hub)?.len();
    if io.json {
        io.event(
            "started",
            vec![
                ("root", J::s(root.to_string_lossy())),
                ("pid", J::n(std::process::id())),
                ("peers", J::n(npeers)),
            ],
        )?;
    } else {
        writeln!(
            io.out,
            "watching {} ({npeers} peer(s)); Ctrl-C to stop",
            root.display(),
        )?;
    }
    io.flush()?;

    // A failed step (e.g. blocked paths, a refused checkout) is reported
    // and retried on the next trigger; it never stops the watcher.
    let fail = |io: &mut Io, stage: &str, e: Error| -> Result<(), Error> {
        if io.json {
            io.event(
                "error",
                vec![("stage", J::s(stage)), ("message", J::s(&e.0))],
            )
        } else {
            io.warning(&format!("{stage}: {e}"))
        }
    };
    let cycle = |do_sync: bool, io: &mut Io| -> Result<(), Error> {
        let recorded = match io.call(|o, w| step(&hub, o, w)) {
            Ok(s) => {
                step_events(io, &s)?;
                s.recorded.is_some()
            }
            Err(e) => {
                fail(io, "auto-record", e)?;
                false
            }
        };
        if (recorded || do_sync) && !stopping() {
            let mid = oplog_len(&hub);
            sync_all(&hub, io)?;
            if oplog_len(&hub) != mid && !stopping() {
                match io.call(|o, w| step(&hub, o, w)) {
                    Ok(s) => step_events(io, &s)?,
                    Err(e) => fail(io, "apply", e)?,
                }
            }
        }
        io.flush()
    };

    cycle(true, &mut io)?;
    let mut base_fp = fingerprint(&root);
    let mut base_len = oplog_len(&hub);
    let mut seen_fp = base_fp;
    let mut last_change = Instant::now();
    let mut next_sync = Instant::now() + opts.sync_every;
    while !stopping() {
        std::thread::sleep(opts.poll);
        if stopping() {
            break;
        }
        let fp = fingerprint(&root);
        if fp != seen_fp {
            seen_fp = fp;
            last_change = Instant::now();
        }
        let quiet = last_change.elapsed() >= opts.debounce;
        let local = fp != base_fp;
        let incoming = oplog_len(&hub) != base_len;
        let periodic = Instant::now() >= next_sync;
        if !quiet || !(local || incoming || periodic) {
            continue;
        }
        cycle(local || periodic, &mut io)?;
        if periodic || local {
            next_sync = Instant::now() + opts.sync_every;
        }
        // Self-write suppression: adopt what the step left as the baseline.
        base_fp = fingerprint(&root);
        seen_fp = base_fp;
        base_len = oplog_len(&hub);
    }
    if io.json {
        io.event("stopped", vec![])?;
    } else {
        writeln!(io.out, "watch stopped")?;
    }
    io.flush()
}
