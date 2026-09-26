//! Working directory (Issue #19): checkout index, scan, diff, checkout plan.
//!
//! The working directory is the repo root (the parent of `.hub/`), excluding
//! the top-level `.hub/` entry. Symlinks and special files are skipped with a
//! warning; non-UTF-8 names are skipped with a warning.
//!
//! `.hub/index` (format v0 [decision-proposal]), UTF-8 text written with
//! temp + fsync + rename:
//! ```text
//! barocss-hub/index/v0
//! base <frontier ids, comma-joined, or ->
//! d <esc path> <node id>
//! f <esc path> <node id> <blob hex> <size> <mtime ns>
//! ```
//! `esc`/ids are the op-log encodings (`store::esc`, `store::fmt_id`). `base`
//! is the frontier of the version last materialized. Size/mtime are the stat
//! of the file as hub last wrote or read it; `0 0` means "unknown, rehash".
//! A file's cached blob is trusted only if size and mtime match *and* its
//! mtime is strictly older than the index file's own mtime (racy-git guard);
//! otherwise it is rehashed.
//!
//! A missing index means nothing was materialized yet: base = genesis, no
//! entries.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use hub_core::projection::Projection;
use hub_core::tree::{BlobId, Kind, NodeId, OpId, State};

use crate::blobs::{BlobError, Blobs};
use crate::store::{self, esc, fmt_id, unesc};
use crate::{Error, HUB_DIR};

pub const INDEX_HEADER: &str = "barocss-hub/index/v0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    Dir,
    File {
        blob: BlobId,
        size: u64,
        mtime: u128,
    },
}

impl Entry {
    fn is_dir(&self) -> bool {
        matches!(self, Entry::Dir)
    }
    fn blob(&self) -> Option<BlobId> {
        match self {
            Entry::File { blob, .. } => Some(*blob),
            Entry::Dir => None,
        }
    }
    /// Same kind and (for files) same bytes.
    fn same_content(&self, o: &Entry) -> bool {
        match (self, o) {
            (Entry::Dir, Entry::Dir) => true,
            (Entry::File { blob: a, .. }, Entry::File { blob: b, .. }) => a == b,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Index {
    /// Frontier of the materialized version (sorted).
    pub base: Vec<OpId>,
    pub entries: BTreeMap<String, (NodeId, Entry)>,
    /// mtime (ns) of the index file when loaded; 0 if absent.
    pub stamp: u128,
}

fn mtime_ns(m: &fs::Metadata) -> u128 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn parse_blob(t: &str) -> Result<BlobId, String> {
    if t.len() != 64 {
        return Err("bad blob".into());
    }
    let mut b = [0u8; 32];
    for (i, o) in b.iter_mut().enumerate() {
        *o = u8::from_str_radix(&t[2 * i..2 * i + 2], 16).map_err(|_| "bad blob")?;
    }
    Ok(BlobId(b))
}

fn parse_id(t: &str) -> Result<NodeId, String> {
    let (c, r) = t.split_once('.').ok_or("bad id")?;
    if r.len() != 32 {
        return Err("bad id".into());
    }
    Ok(NodeId::new(
        c.parse().map_err(|_| "bad id")?,
        u128::from_str_radix(r, 16).map_err(|_| "bad id")?,
    ))
}

impl Index {
    pub fn path(hub_dir: &Path) -> PathBuf {
        hub_dir.join("index")
    }

    pub fn load(hub_dir: &Path) -> Result<Index, Error> {
        let p = Self::path(hub_dir);
        let text = match fs::read_to_string(&p) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Index::default()),
            Err(e) => return Err(Error(format!("cannot read {}: {e}", p.display()))),
        };
        let stamp = fs::metadata(&p).map(|m| mtime_ns(&m)).unwrap_or(0);
        let bad = |line: usize, why: &str| {
            Error(format!(
                "corrupt {} line {line}: {why} (repair: `hub checkout --force`)",
                p.display()
            ))
        };
        let mut lines = text.lines();
        if lines.next() != Some(INDEX_HEADER) {
            return Err(bad(1, "missing or wrong header"));
        }
        let base = match lines.next().and_then(|l| l.strip_prefix("base ")) {
            Some("-") => Vec::new(),
            Some(ids) => ids
                .split(',')
                .map(parse_id)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|w| bad(2, &w))?,
            None => return Err(bad(2, "missing base")),
        };
        let mut entries = BTreeMap::new();
        for (i, l) in lines.enumerate() {
            let t: Vec<&str> = l.split(' ').collect();
            let r: Result<(String, NodeId, Entry), String> = (|| match t.as_slice() {
                ["d", p, n] => Ok((unesc(p)?, parse_id(n)?, Entry::Dir)),
                ["f", p, n, b, s, m] => Ok((
                    unesc(p)?,
                    parse_id(n)?,
                    Entry::File {
                        blob: parse_blob(b)?,
                        size: s.parse().map_err(|_| "bad size")?,
                        mtime: m.parse().map_err(|_| "bad mtime")?,
                    },
                )),
                _ => Err("bad entry".into()),
            })();
            let (path, node, e) = r.map_err(|w| bad(i + 3, &w))?;
            entries.insert(path, (node, e));
        }
        let mut base = base;
        base.sort();
        Ok(Index {
            base,
            entries,
            stamp,
        })
    }

    pub fn save(&self, hub_dir: &Path) -> Result<(), Error> {
        let mut s = format!("{INDEX_HEADER}\nbase ");
        if self.base.is_empty() {
            s.push('-');
        } else {
            let ids: Vec<String> = self.base.iter().map(|i| fmt_id(*i)).collect();
            s.push_str(&ids.join(","));
        }
        s.push('\n');
        for (p, (n, e)) in &self.entries {
            match e {
                Entry::Dir => s.push_str(&format!("d {} {}\n", esc(p), fmt_id(*n))),
                Entry::File { blob, size, mtime } => s.push_str(&format!(
                    "f {} {} {} {size} {mtime}\n",
                    esc(p),
                    fmt_id(*n),
                    blob.hex()
                )),
            }
        }
        store::write_atomic(&Self::path(hub_dir), s.as_bytes())?;
        Ok(())
    }
}

/// The current working directory contents.
pub type Scan = BTreeMap<String, Entry>;

/// Walk `root` (excluding top-level `.hub`), hashing files unless the index
/// stat cache is trustworthy.
pub fn scan(root: &Path, idx: &Index, warnings: &mut Vec<String>) -> Result<Scan, Error> {
    let mut out = Scan::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        for e in fs::read_dir(&dir)? {
            let e = e?;
            let name = e.file_name();
            let Some(name) = name.to_str() else {
                warnings.push(format!(
                    "skipping non-UTF-8 name {:?} in '{rel}'",
                    e.file_name()
                ));
                continue;
            };
            if rel.is_empty() && name == HUB_DIR {
                continue;
            }
            let path = if rel.is_empty() {
                name.to_string()
            } else {
                format!("{rel}/{name}")
            };
            let md = fs::symlink_metadata(e.path())?;
            let ft = md.file_type();
            if ft.is_symlink() {
                warnings.push(format!("skipping symlink '{path}' (unsupported in v0)"));
            } else if ft.is_dir() {
                out.insert(path.clone(), Entry::Dir);
                stack.push((e.path(), path));
            } else if ft.is_file() {
                let (size, mtime) = (md.len(), mtime_ns(&md));
                let cached = match idx.entries.get(&path) {
                    Some((
                        _,
                        Entry::File {
                            blob,
                            size: s,
                            mtime: m,
                        },
                    )) if *s == size && *m == mtime && mtime < idx.stamp => Some(*blob),
                    _ => None,
                };
                let blob = match cached {
                    Some(b) => b,
                    None => BlobId::of(&fs::read(e.path())?),
                };
                out.insert(path, Entry::File { blob, size, mtime });
            } else {
                warnings.push(format!("skipping special file '{path}'"));
            }
        }
    }
    Ok(out)
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    Added(String),
    Modified(String),
    Deleted(String),
    /// Rename detection v0 [decision-proposal]: one deleted file and one
    /// added file with the same blob, and no other deleted/added file with
    /// that blob.
    Renamed(String, String),
}

/// Changes from the index to the scan.
pub fn diff(idx: &Index, w: &Scan) -> Vec<Change> {
    let mut added = Vec::new();
    let mut deleted = Vec::new();
    let mut out = Vec::new();
    for (p, (_, ie)) in &idx.entries {
        match w.get(p) {
            None => deleted.push(p.clone()),
            Some(we) if we.is_dir() != ie.is_dir() => {
                deleted.push(p.clone());
                added.push(p.clone());
            }
            Some(we) if !we.same_content(ie) => out.push(Change::Modified(p.clone())),
            Some(_) => {}
        }
    }
    for p in w.keys() {
        if !idx.entries.contains_key(p) {
            added.push(p.clone());
        }
    }
    // Rename detection over files only.
    let mut by_blob: BTreeMap<BlobId, (Vec<String>, Vec<String>)> = BTreeMap::new();
    for p in &deleted {
        if let Some(b) = idx.entries[p].1.blob() {
            by_blob.entry(b).or_default().0.push(p.clone());
        }
    }
    for p in &added {
        if let Some(b) = w[p].blob() {
            by_blob.entry(b).or_default().1.push(p.clone());
        }
    }
    let mut renamed_from = BTreeSet::new();
    let mut renamed_to = BTreeSet::new();
    for (d, a) in by_blob.values() {
        if let ([d], [a]) = (d.as_slice(), a.as_slice()) {
            if d != a {
                out.push(Change::Renamed(d.clone(), a.clone()));
                renamed_from.insert(d.clone());
                renamed_to.insert(a.clone());
            }
        }
    }
    out.extend(
        deleted
            .into_iter()
            .filter(|p| !renamed_from.contains(p))
            .map(Change::Deleted),
    );
    out.extend(
        added
            .into_iter()
            .filter(|p| !renamed_to.contains(p))
            .map(Change::Added),
    );
    out.sort();
    out
}

pub fn render_changes(changes: &[Change], idx: &Index, w: &Scan) -> Vec<String> {
    let slash = |p: &str, dir: bool| {
        if dir {
            format!("{p}/")
        } else {
            p.to_string()
        }
    };
    changes
        .iter()
        .map(|c| match c {
            Change::Added(p) => format!("added:    {}", slash(p, w[p].is_dir())),
            Change::Modified(p) => format!("modified: {p}"),
            Change::Deleted(p) => {
                format!("deleted:  {}", slash(p, idx.entries[p].1.is_dir()))
            }
            Change::Renamed(a, b) => format!("renamed:  {a} -> {b}"),
        })
        .collect()
}

/// Probe whether the filesystem holding `hub_dir` folds ASCII case.
pub fn case_insensitive(hub_dir: &Path) -> io::Result<bool> {
    let lower = hub_dir.join(".case-probe");
    let upper = hub_dir.join(".CASE-PROBE");
    fs::write(&lower, b"")?;
    let r = upper.exists();
    fs::remove_file(&lower)?;
    Ok(r)
}

/// Case-folding key for clash detection (Unicode simple lowercase; Unicode
/// normalization is not modeled, see open questions on Issue #19).
fn fold(s: &str) -> String {
    s.to_lowercase()
}

/// What a checkout must do, computed before touching anything.
#[derive(Debug, Default)]
pub struct Plan {
    /// Unrecorded paths deleted or overwritten under `--force`.
    force_remove: Vec<String>,
    remove_files: Vec<String>,
    /// Removed only if empty after file removal; deepest first.
    remove_dirs: Vec<String>,
    make_dirs: Vec<String>,
    write_files: Vec<(String, Vec<u8>)>,
    /// Final index entries; `true` = refresh size/mtime from disk after apply.
    index: BTreeMap<String, (NodeId, Entry, bool)>,
    base: Vec<OpId>,
    pub warnings: Vec<String>,
}

impl Plan {
    /// (paths created or written, paths removed).
    pub fn counts(&self) -> (usize, usize) {
        (
            self.make_dirs.len() + self.write_files.len(),
            self.force_remove.len() + self.remove_files.len() + self.remove_dirs.len(),
        )
    }
}

pub struct Target<'a> {
    pub state: &'a State,
    pub proj: &'a Projection,
    pub frontier: Vec<OpId>,
}

pub struct PlanOpts {
    pub force: bool,
    /// Rewrite tracked files missing from disk even if the target does not
    /// change them (true for `checkout`, false for mkdir/touch/mv/rm).
    pub restore_missing: bool,
    pub case_insensitive: bool,
}

/// Plan materializing `t` over the working directory `w` last written as
/// `idx`. Refuses (Err) on case clashes and, unless `force`, on unrecorded
/// changes that would be overwritten or deleted.
pub fn plan(
    t: &Target,
    idx: &Index,
    w: &Scan,
    blobs: &Blobs,
    opts: &PlanOpts,
) -> Result<Plan, Error> {
    let mut p = Plan {
        base: {
            let mut b = t.frontier.clone();
            b.sort();
            b
        },
        ..Plan::default()
    };
    // Target entries.
    let mut tgt: BTreeMap<String, (NodeId, Option<BlobId>)> = BTreeMap::new();
    for (path, node) in &t.proj.entries {
        if path.is_empty() {
            continue;
        }
        let blob = match t.state[node].kind {
            Kind::File => Some(
                t.proj
                    .blobs
                    .get(node)
                    .copied()
                    .unwrap_or_else(BlobId::empty),
            ),
            _ => None,
        };
        tgt.insert(path.clone(), (*node, blob));
    }
    // D-003: byte-exact names; on a case-insensitive FS a clash is refused.
    if opts.case_insensitive {
        let mut seen: BTreeMap<String, &str> = BTreeMap::new();
        let mut clashes = Vec::new();
        for path in tgt.keys() {
            if let Some(prev) = seen.insert(fold(path), path) {
                clashes.push(format!("'{prev}' and '{path}'"));
            }
        }
        if !clashes.is_empty() {
            return Err(Error(format!(
                "case clash on a case-insensitive filesystem: {} (hub names are byte-exact, D-003; \
                 rename one with `hub mv` in a case-sensitive checkout or edit the log)",
                clashes.join(", ")
            )));
        }
    }
    let t_entry_matches = |path: &str, e: &Entry| -> bool {
        match (tgt.get(path), e) {
            (Some((_, None)), Entry::Dir) => true,
            (Some((_, Some(b))), Entry::File { blob, .. }) => b == blob,
            _ => false,
        }
    };
    // Tracked and clean: on disk equals what hub last wrote.
    let clean = |path: &str| -> bool {
        match (idx.entries.get(path), w.get(path)) {
            (Some((_, ie)), Some(we)) => ie.same_content(we),
            _ => false,
        }
    };
    let mut refused = Vec::new();
    let mut removing: BTreeSet<String> = BTreeSet::new();
    // 1. Tracked paths no longer in the target (or changing kind).
    for (path, (_, ie)) in &idx.entries {
        let stays = match tgt.get(path) {
            Some((_, b)) => b.is_none() == ie.is_dir(),
            None => false,
        };
        if stays {
            continue;
        }
        match w.get(path) {
            None => {}
            Some(we) if we.is_dir() => {
                if ie.is_dir() {
                    p.remove_dirs.push(path.clone());
                    removing.insert(path.clone());
                } else if opts.force {
                    p.force_remove.push(path.clone());
                    removing.insert(path.clone());
                } else {
                    refused.push(format!("'{path}' was replaced by a directory"));
                }
            }
            Some(_) => {
                if clean(path) {
                    p.remove_files.push(path.clone());
                    removing.insert(path.clone());
                } else if opts.force {
                    p.force_remove.push(path.clone());
                    removing.insert(path.clone());
                } else {
                    refused.push(format!(
                        "'{path}' has unrecorded changes and would be deleted"
                    ));
                }
            }
        }
    }
    let wfold: BTreeMap<String, &String> = w.keys().map(|k| (fold(k), k)).collect();
    // A path is "gone" if it or an ancestor dir is being removed.
    let gone = |removing: &BTreeSet<String>, path: &str| -> bool {
        removing.contains(path)
            || path
                .match_indices('/')
                .any(|(i, _)| removing.contains(&path[..i]))
    };
    // 2. Target paths, shallow first (BTreeMap order puts parents first).
    for (path, (node, blob)) in &tgt {
        let we = w.get(path).filter(|_| !gone(&removing, path));
        // Case-insensitive: an on-disk sibling differing only by case is in
        // the way unless it is being removed.
        if we.is_none() && opts.case_insensitive {
            if let Some(q) = wfold.get(&fold(path)) {
                if *q != path && !gone(&removing, q) && !tgt.contains_key(*q) {
                    if opts.force {
                        p.force_remove.push((*q).clone());
                        removing.insert((*q).clone());
                    } else {
                        refused.push(format!(
                            "'{q}' is in the way of '{path}' (case-insensitive)"
                        ));
                        continue;
                    }
                }
            }
        }
        match blob {
            None => {
                match we {
                    Some(Entry::Dir) => {}
                    Some(_) => {
                        if opts.force {
                            p.force_remove.push(path.clone());
                            removing.insert(path.clone());
                            p.make_dirs.push(path.clone());
                        } else {
                            refused.push(format!("untracked file '{path}' is in the way"));
                            continue;
                        }
                    }
                    None => p.make_dirs.push(path.clone()),
                }
                p.index.insert(path.clone(), (*node, Entry::Dir, true));
            }
            Some(b) => {
                let ie = idx.entries.get(path).map(|(_, e)| *e);
                let unchanged_by_target = ie.is_some_and(|ie| ie.blob() == Some(*b));
                let write = match we {
                    Some(e) if t_entry_matches(path, e) => false,
                    None => opts.restore_missing || !unchanged_by_target,
                    Some(Entry::Dir) => {
                        // In the way: a directory with anything not removed.
                        let busy = w.keys().any(|k| {
                            k.len() > path.len()
                                && k.starts_with(path.as_str())
                                && k.as_bytes()[path.len()] == b'/'
                                && !gone(&removing, k)
                        });
                        if opts.force || !busy {
                            p.force_remove.push(path.clone());
                            removing.insert(path.clone());
                            true
                        } else {
                            refused.push(format!("directory '{path}' is in the way"));
                            continue;
                        }
                    }
                    Some(_) if clean(path) => true,
                    Some(_) if opts.force => true,
                    Some(_) if unchanged_by_target => {
                        // Modified locally, target keeps the same content:
                        // leave the local edit alone, stays "modified".
                        p.index.insert(
                            path.clone(),
                            (
                                *node,
                                Entry::File {
                                    blob: *b,
                                    size: 0,
                                    mtime: 0,
                                },
                                false,
                            ),
                        );
                        continue;
                    }
                    Some(_) => {
                        refused.push(format!(
                            "'{path}' has unrecorded changes and would be overwritten"
                        ));
                        continue;
                    }
                };
                if write {
                    let bytes = blobs.get(b).map_err(|e| match e {
                        BlobError::Missing(_) => Error(format!(
                            "cannot check out '{path}': {e} (content not available locally)"
                        )),
                        e => Error(format!("cannot check out '{path}': {e}")),
                    })?;
                    p.write_files.push((path.clone(), bytes));
                    p.index.insert(
                        path.clone(),
                        (
                            *node,
                            Entry::File {
                                blob: *b,
                                size: 0,
                                mtime: 0,
                            },
                            true,
                        ),
                    );
                } else if we.is_none() {
                    // Missing and not restored: keep it "deleted".
                    p.index.insert(
                        path.clone(),
                        (
                            *node,
                            Entry::File {
                                blob: *b,
                                size: 0,
                                mtime: 0,
                            },
                            false,
                        ),
                    );
                } else {
                    p.index.insert(
                        path.clone(),
                        (
                            *node,
                            Entry::File {
                                blob: *b,
                                size: 0,
                                mtime: 0,
                            },
                            true,
                        ),
                    );
                }
            }
        }
    }
    if !refused.is_empty() {
        return Err(Error(format!(
            "refusing to check out (use --force to discard unrecorded changes):\n  {}",
            refused.join("\n  ")
        )));
    }
    p.remove_dirs.sort_by(|a, b| b.cmp(a));
    Ok(p)
}

/// Crash-safe working-file write: `.<name>.hub-tmp` in the same directory,
/// fsync, rename over the target.
fn write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.hub-tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// Execute a plan under `root` and write the new index.
pub fn apply(p: Plan, root: &Path, hub_dir: &Path) -> Result<Vec<String>, Error> {
    let mut warnings = p.warnings;
    for path in &p.force_remove {
        let fp = root.join(path);
        match fs::symlink_metadata(&fp) {
            Ok(m) if m.is_dir() => fs::remove_dir_all(&fp)?,
            Ok(_) => fs::remove_file(&fp)?,
            Err(_) => {}
        }
    }
    for path in &p.remove_files {
        match fs::remove_file(root.join(path)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    for path in &p.remove_dirs {
        match fs::remove_dir(root.join(path)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(_) => warnings.push(format!(
                "kept directory '{path}': it still contains untracked files"
            )),
        }
    }
    for path in &p.make_dirs {
        fs::create_dir_all(root.join(path))?;
    }
    for (path, bytes) in &p.write_files {
        write_file(&root.join(path), bytes)?;
    }
    let mut idx = Index {
        base: p.base,
        ..Index::default()
    };
    for (path, (node, e, refresh)) in p.index {
        let e = match (e, refresh) {
            (Entry::File { blob, .. }, true) => {
                let md = fs::metadata(root.join(&path))?;
                Entry::File {
                    blob,
                    size: md.len(),
                    mtime: mtime_ns(&md),
                }
            }
            (e, _) => e,
        };
        idx.entries.insert(path, (node, e));
    }
    idx.save(hub_dir)?;
    Ok(warnings)
}
