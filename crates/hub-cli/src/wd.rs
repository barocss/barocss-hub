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
//! u <esc path> <node id>
//! ```
//! A `u` entry is a head path deliberately **not materialized** because of a
//! case clash on a case-insensitive filesystem (D-019, Issue #20): it is not
//! on disk, is never reported as deleted, and is listed by `hub status`.
//! `esc`/ids are the op-log encodings (`store::esc`, `store::fmt_id`). `base`
//! is the frontier of the version last materialized. Size/mtime are the stat
//! of the file as hub last wrote or read it; `0 0` means "unknown, rehash".
//! A file's cached blob is trusted only if size and mtime match *and* its
//! mtime is strictly older than the index file's own mtime (racy-git guard);
//! otherwise it is rehashed. On save, entries whose mtime is within
//! `RACY_NS` of the save time are smudged to `0 0` (git's racy-clean fix for
//! coarse-mtime filesystems).
//!
//! Temps for working files live in `.hub/tmp/` (never scanned); an
//! interrupted checkout leaves `.hub/checkout-in-progress`, see [`apply`].
//!
//! A missing index means nothing was materialized yet: base = genesis, no
//! entries.

use crate::Code;
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
/// Racy window (ns): covers 2 s mtime granularity (FAT/HFS+/some network fs).
pub const RACY_NS: u128 = 2_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    Dir,
    File {
        blob: BlobId,
        size: u64,
        mtime: u128,
    },
    /// Skipped at checkout because of a case clash (D-019).
    Unmaterialized,
}

impl Entry {
    fn is_dir(&self) -> bool {
        matches!(self, Entry::Dir)
    }
    fn blob(&self) -> Option<BlobId> {
        match self {
            Entry::File { blob, .. } => Some(*blob),
            Entry::Dir | Entry::Unmaterialized => None,
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
    crate::store::parse_blob(t)
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

    /// Issue #56: the index a record that authors `TextEdit`s installs
    /// once its ops are durable (two-phase: written before the append,
    /// promoted after it). See `crate::load_index`.
    pub fn next_path(hub_dir: &Path) -> PathBuf {
        hub_dir.join("index.next")
    }

    pub fn load(hub_dir: &Path) -> Result<Index, Error> {
        Self::load_from(&Self::path(hub_dir))
    }

    /// [`Index::load`] of the index file at `p`.
    pub fn load_from(p: &Path) -> Result<Index, Error> {
        let p = p.to_path_buf();
        let text = match fs::read_to_string(&p) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Index::default()),
            Err(e) => {
                return Err(Error::new(
                    Code::Io,
                    format!("cannot read {}: {e}", p.display()),
                ))
            }
        };
        let stamp = fs::metadata(&p).map(|m| mtime_ns(&m)).unwrap_or(0);
        let bad = |line: usize, why: &str| {
            Error::new(
                Code::Corrupt,
                format!(
                    "corrupt {} line {line}: {why} (repair: `barocss-hub checkout --force`)",
                    p.display()
                ),
            )
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
                ["u", p, n] => Ok((unesc(p)?, parse_id(n)?, Entry::Unmaterialized)),
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

    /// Writes the index. Racy-git smudging: a file entry whose mtime is
    /// within [`RACY_NS`] of now is stored as `0 0` (rehash next time), so
    /// an edit landing in the same coarse mtime tick as hub's stat can never
    /// be trusted as clean.
    pub fn save(&self, hub_dir: &Path) -> Result<(), Error> {
        self.save_to(&Self::path(hub_dir))
    }

    /// [`Index::save`] to the file `p` (durable atomic replace).
    pub fn save_to(&self, p: &Path) -> Result<(), Error> {
        let now = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(u128::MAX);
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
                Entry::Unmaterialized => s.push_str(&format!("u {} {}\n", esc(p), fmt_id(*n))),
                Entry::File { blob, size, mtime } => {
                    let (size, mtime) = if mtime.saturating_add(RACY_NS) >= now {
                        (&0, &0)
                    } else {
                        (size, mtime)
                    };
                    s.push_str(&format!(
                        "f {} {} {} {size} {mtime}\n",
                        esc(p),
                        fmt_id(*n),
                        blob.hex()
                    ))
                }
            }
        }
        store::write_atomic(p, s.as_bytes())?;
        Ok(())
    }
}

/// The current working directory contents.
pub type Scan = BTreeMap<String, Entry>;

/// Untracked working-directory paths matched by `.hubignore` (Issue #24).
pub type Ignored = BTreeSet<String>;

/// Stand-in blob for ignored files, which are never hashed. It cannot equal
/// a real content hash in practice, and [`plan`] refuses to touch ignored
/// paths before comparing blobs.
const IGNORED_BLOB: BlobId = BlobId([0xff; 32]);

fn has_tracked_under(idx: &Index, dir: &str) -> bool {
    let lo = format!("{dir}/");
    idx.entries
        .range(lo.clone()..)
        .next()
        .is_some_and(|(k, _)| k.starts_with(&lo))
}

/// Walk `root` (excluding top-level `.hub`) and return only the paths that
/// are tracked or not ignored: what `status` and `record` see.
pub fn scan(root: &Path, idx: &Index, warnings: &mut Vec<String>) -> Result<Scan, Error> {
    let (mut w, ign) = scan_all(root, idx, warnings)?;
    w.retain(|p, _| !ign.contains(p));
    Ok(w)
}

/// Walk `root` (excluding top-level `.hub`), hashing files unless the index
/// stat cache is trustworthy. The scan includes untracked ignored paths
/// (listed again in the returned set): ignored files are not hashed, and an
/// ignored untracked directory is not descended unless tracked paths lie
/// under it. Tracked paths are never classed as ignored ([decision-proposal],
/// #24: once tracked, a path stays tracked even if a rule matches it).
pub fn scan_all(
    root: &Path,
    idx: &Index,
    warnings: &mut Vec<String>,
) -> Result<(Scan, Ignored), Error> {
    let ig = crate::ignore::Ignore::load(root)?;
    let mut out = Scan::new();
    let mut ignored = Ignored::new();
    // (dir, rel, an ancestor dir is ignored)
    let mut stack = vec![(root.to_path_buf(), String::new(), false)];
    while let Some((dir, rel, under_ignored)) = stack.pop() {
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
            let tracked = idx.entries.contains_key(&path);
            let matched = under_ignored || ig.is_ignored_here(&path, ft.is_dir());
            let is_ign = matched && !tracked;
            if ft.is_symlink() {
                if !is_ign {
                    warnings.push(format!("skipping symlink '{path}' (unsupported in v0)"));
                }
            } else if ft.is_dir() {
                out.insert(path.clone(), Entry::Dir);
                if is_ign {
                    ignored.insert(path.clone());
                }
                if !is_ign || has_tracked_under(idx, &path) {
                    stack.push((e.path(), path, matched));
                }
            } else if ft.is_file() {
                let (size, mtime) = (md.len(), mtime_ns(&md));
                if is_ign {
                    ignored.insert(path.clone());
                    out.insert(
                        path,
                        Entry::File {
                            blob: IGNORED_BLOB,
                            size,
                            mtime,
                        },
                    );
                    continue;
                }
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
            } else if !is_ign {
                warnings.push(format!("skipping special file '{path}'"));
            }
        }
    }
    Ok((out, ignored))
}

/// Tracked paths (index entries) that an ignore rule matches: they stay
/// tracked ([decision-proposal], #24) and `status` notes them.
pub fn tracked_but_ignored(root: &Path, idx: &Index) -> Result<Vec<String>, Error> {
    let ig = crate::ignore::Ignore::load(root)?;
    Ok(idx
        .entries
        .iter()
        .filter(|(p, (_, e))| ig.is_ignored(p, e.is_dir()))
        .map(|(p, _)| p.clone())
        .collect())
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

/// A working-directory path that occupies an unmaterialized (D-019) path:
/// equal to one, case-folding onto one, or under such a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocked {
    pub path: String,
    /// The unmaterialized index paths it collides with, with their nodes.
    pub with: Vec<(String, NodeId)>,
}

/// Scanned paths that collide with unmaterialized index entries (#20
/// repair). Such paths are excluded from [`diff`]; `status` reports them and
/// `record` refuses while any exist.
pub fn blocked(idx: &Index, w: &Scan) -> Vec<Blocked> {
    let unmat: Vec<(String, &String, NodeId)> = idx
        .entries
        .iter()
        .filter(|(_, (_, e))| *e == Entry::Unmaterialized)
        .map(|(p, (n, _))| (fold(p), p, *n))
        .collect();
    if unmat.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for p in w.keys() {
        let fp = fold(p);
        let with: Vec<(String, NodeId)> = unmat
            .iter()
            .filter(|(fq, _, _)| {
                fp == *fq || (fp.starts_with(fq.as_str()) && fp.as_bytes()[fq.len()] == b'/')
            })
            .map(|(_, q, n)| ((*q).clone(), *n))
            .collect();
        if !with.is_empty() {
            out.push(Blocked {
                path: p.clone(),
                with,
            });
        }
    }
    out
}

/// Changes from the index to the scan. Paths reported by [`blocked`] are
/// not changes.
pub fn diff(idx: &Index, w: &Scan) -> Vec<Change> {
    let blocked: BTreeSet<String> = blocked(idx, w).into_iter().map(|b| b.path).collect();
    let mut added = Vec::new();
    let mut deleted = Vec::new();
    let mut out = Vec::new();
    for (p, (_, ie)) in &idx.entries {
        if *ie == Entry::Unmaterialized {
            continue;
        }
        match w.get(p) {
            None => deleted.push(p.clone()),
            Some(_) if blocked.contains(p) => {}
            Some(we) if we.is_dir() != ie.is_dir() => {
                deleted.push(p.clone());
                added.push(p.clone());
            }
            Some(we) if !we.same_content(ie) => out.push(Change::Modified(p.clone())),
            Some(_) => {}
        }
    }
    for p in w.keys() {
        if !idx.entries.contains_key(p) && !blocked.contains(p) {
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

pub fn render_blocked(b: &[Blocked]) -> Vec<String> {
    b.iter()
        .map(|b| {
            let with: Vec<String> = b
                .with
                .iter()
                .map(|(q, n)| format!("'{q}' ({})", fmt_id(*n)))
                .collect();
            format!(
                "blocked:  {} collides with unmaterialized {}",
                b.path,
                with.join(", ")
            )
        })
        .collect()
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

/// Test-only hook: when set, [`case_insensitive`] reports `true` without
/// probing, so case-clash handling (D-019) can be tested on a case-sensitive
/// filesystem. Clash detection is purely name-based, so the only difference
/// from a real case-insensitive filesystem is that both names could exist.
#[doc(hidden)]
pub const FORCE_CI_ENV: &str = "HUB_TEST_FORCE_CASE_INSENSITIVE";

/// Probe whether the filesystem holding `hub_dir` folds ASCII case.
pub fn case_insensitive(hub_dir: &Path) -> io::Result<bool> {
    if std::env::var_os(FORCE_CI_ENV).is_some() {
        return Ok(true);
    }
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

/// D-019: the paths of `paths` that cannot be materialized on a
/// case-insensitive filesystem: every member of a group of paths equal under
/// [`fold`] (no member is privileged), plus everything under such a member.
/// Empty when `case_insensitive` is false.
pub fn clashing<'a>(
    paths: impl IntoIterator<Item = &'a String> + Clone,
    case_insensitive: bool,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if !case_insensitive {
        return out;
    }
    let mut groups: BTreeMap<String, Vec<&String>> = BTreeMap::new();
    for p in paths.clone() {
        groups.entry(fold(p)).or_default().push(p);
    }
    let roots: BTreeSet<&str> = groups
        .values()
        .filter(|g| g.len() > 1)
        .flatten()
        .map(|p| p.as_str())
        .collect();
    for p in paths {
        let hit = roots.contains(p.as_str())
            || p.match_indices('/').any(|(i, _)| roots.contains(&p[..i]));
        if hit {
            out.insert(p.clone());
        }
    }
    out
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
    /// Issue #25 repair (D-020): `Some(pin)` makes [`apply`] write
    /// (`Some(frontier)`) or clear (`None`) `.hub/pinned` after the index is
    /// saved and *before* the marker is removed; `None` leaves it alone.
    pub pin: Option<Option<Vec<OpId>>>,
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
    /// Untracked ignored paths in `w` (from [`scan_all`]): never written
    /// over or deleted, even under `force` (Issue #24).
    pub ignored: Ignored,
    /// Command named in a refusal: `"check out"`, `"mkdir"`, `"mv"`, ...
    pub verb: &'static str,
}

/// Plan materializing `t` over the working directory `w` last written as
/// `idx`. Case-clashing paths are skipped and indexed as unmaterialized
/// (D-019); refuses (Err), unless `force`, on unrecorded changes that would
/// be overwritten or deleted.
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
    // D-019 (refines D-003): on a case-insensitive FS, skip only the
    // clashing paths, index them as unmaterialized, materialize the rest.
    for path in clashing(tgt.keys(), opts.case_insensitive) {
        let (node, _) = tgt.remove(&path).expect("clash path is a target path");
        p.warnings.push(format!(
            "not materialized (case clash on a case-insensitive filesystem, D-019): '{path}'"
        ));
        p.index.insert(path, (node, Entry::Unmaterialized, false));
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
        if *ie == Entry::Unmaterialized {
            continue;
        }
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
        if opts.ignored.contains(path) {
            refused.push(format!(
                "ignored untracked '{path}' is in the way (never deleted, even with --force; move it away)"
            ));
            continue;
        }
        // A tracked directory being removed (non-recursively) and replaced
        // by a file must be empty of untracked leftovers, or the file write
        // would fail half-way (found while testing #24).
        if blob.is_some() && removing.contains(path) && w.get(path).is_some_and(Entry::is_dir) {
            let pre = format!("{path}/");
            let left = w
                .keys()
                .find(|k| k.starts_with(&pre) && !idx.entries.contains_key(*k));
            if let Some(k) = left {
                if opts.force {
                    p.force_remove.push(path.clone());
                } else {
                    refused.push(format!(
                        "untracked '{k}' is inside directory '{path}', which would be replaced by a file"
                    ));
                    continue;
                }
            }
        }
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
                        BlobError::Missing(_) => Error::new(
                            Code::NoSuchBlob,
                            format!(
                                "cannot check out '{path}': {e} (content not available locally)"
                            ),
                        ),
                        e => crate::blob_error(e).prefix(&format!("cannot check out '{path}'")),
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
    // Ignored untracked files are never deleted, even with --force: refuse
    // any forced removal that is, or contains, an ignored path.
    for f in &p.force_remove {
        let pre = format!("{f}/");
        if let Some(i) = opts.ignored.iter().find(|i| *i == f || i.starts_with(&pre)) {
            refused.push(format!(
                "ignored untracked '{i}' would be deleted (never deleted, even with --force; move it away)"
            ));
        }
    }
    if !refused.is_empty() {
        let hint = if opts.verb == "check out" && opts.force {
            " (these paths are protected even with --force)"
        } else if opts.verb == "check out" {
            " (use --force to discard unrecorded changes)"
        } else {
            " (it would overwrite or delete unrecorded working-directory changes; nothing was appended)"
        };
        return Err(Error::new(
            Code::Refused,
            format!(
                "refusing to {}{hint}:\n  {}",
                opts.verb,
                refused.join("\n  ")
            ),
        ));
    }
    p.remove_dirs.sort_by(|a, b| b.cmp(a));
    Ok(p)
}

/// Directory (under `.hub/`) holding in-flight working-file temps.
pub const TMP_DIR: &str = "tmp";
/// Checkout-in-progress marker (under `.hub/`), see [`apply`].
pub const MARKER: &str = "checkout-in-progress";
/// Test-only fault injection: when set, [`apply`] aborts the process right
/// after writing the first working file (marker present, index not saved).
#[doc(hidden)]
pub const CRASH_ENV: &str = "HUB_TEST_CRASH_IN_CHECKOUT";
/// [`CRASH_ENV`] stage values: abort after the index save (before the pin
/// write), or after the pin write (before the marker removal). Any other
/// value aborts after the first working file.
pub const CRASH_AFTER_INDEX: &str = "after-index";
pub const CRASH_AFTER_PIN: &str = "after-pin";

/// Remove leftover working-file temps under `.hub/tmp/`. Callers hold the
/// exclusive repo lock, so no live writer owns them.
pub fn sweep_tmp(hub_dir: &Path) -> io::Result<usize> {
    let dir = hub_dir.join(TMP_DIR);
    let rd = match fs::read_dir(&dir) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut n = 0;
    for e in rd {
        let e = e?;
        fs::remove_file(e.path())?;
        n += 1;
    }
    Ok(n)
}

/// Frontier recorded by an interrupted checkout, if its marker exists.
pub fn interrupted(hub_dir: &Path) -> Result<Option<Vec<OpId>>, Error> {
    let p = hub_dir.join(MARKER);
    let text = match fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let ids = text.lines().nth(1).and_then(|l| l.strip_prefix("target "));
    let f = match ids {
        Some("-") => Vec::new(),
        Some(ids) => ids
            .split(',')
            .map(parse_id)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|w| Error::new(Code::Corrupt, format!("corrupt {}: {w}", p.display())))?,
        None => {
            return Err(Error::new(
                Code::Corrupt,
                format!("corrupt {}", p.display()),
            ))
        }
    };
    Ok(Some(f))
}

fn fmt_frontier(f: &[OpId]) -> String {
    if f.is_empty() {
        "-".into()
    } else {
        f.iter().map(|i| fmt_id(*i)).collect::<Vec<_>>().join(",")
    }
}

/// Crash-safe working-file temp. The temp is a fresh, uniquely named file
/// in `.hub/tmp/` created with `create_new` (O_EXCL), so it never truncates
/// or replaces anything, and is never part of the working directory (scans
/// skip `.hub/`). It is flushed but not barriered: [`apply`] issues one
/// barrier over all temps, then renames each over its target (Issue #29).
/// `.hub/` lives inside the working directory; [`apply`] checks both are on
/// the same device so the rename is atomic.
fn write_temp(tmp_dir: &Path, bytes: &[u8]) -> io::Result<(PathBuf, fs::File)> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    loop {
        let tmp = tmp_dir.join(format!(
            "w-{}-{nanos}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let mut f = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let r = f.write_all(bytes).and_then(|_| crate::durable::flush(&f));
        if let Err(e) = r {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        return Ok((tmp, f));
    }
}

#[cfg(unix)]
fn same_device(a: &Path, b: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    Ok(fs::metadata(a)?.dev() == fs::metadata(b)?.dev())
}
#[cfg(not(unix))]
fn same_device(_: &Path, _: &Path) -> io::Result<bool> {
    Ok(true)
}

/// Execute a plan under `root` and write the new index.
///
/// Before any working-directory change, `.hub/checkout-in-progress` is
/// written durably (it names the target frontier); it is removed only after
/// the new index is saved. While it exists, record and mkdir/touch/mv/rm
/// refuse and `hub checkout` (no id) resumes the interrupted target.
pub fn apply(p: Plan, root: &Path, hub_dir: &Path) -> Result<Vec<String>, Error> {
    let mut warnings = p.warnings;
    let tmp_dir = hub_dir.join(TMP_DIR);
    fs::create_dir_all(&tmp_dir)?;
    if !same_device(root, &tmp_dir)? {
        return Err(Error::new(Code::Io, format!(
            "{} is on a different filesystem than the working directory; atomic writes need both on one filesystem",
            tmp_dir.display()
        )));
    }
    let marker = hub_dir.join(MARKER);
    store::write_atomic(
        &marker,
        format!(
            "barocss-hub/checkout/v0\ntarget {}\n",
            fmt_frontier(&p.base)
        )
        .as_bytes(),
    )?;
    let stage = std::env::var(CRASH_ENV).ok();
    let crash = stage
        .as_deref()
        .is_some_and(|v| v != CRASH_AFTER_INDEX && v != CRASH_AFTER_PIN);
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
    // #29 repair: every directory whose entries this apply changes. A
    // removed path changes its parent; a created directory changes the
    // parent of every level of its chain (create_dir_all may create them
    // all); a written file changes its parent. Flushed before the index.
    let mut dirty: BTreeSet<PathBuf> = BTreeSet::new();
    let parent_of = |rel: &str| -> PathBuf {
        match rel.rsplit_once('/') {
            Some((d, _)) => root.join(d),
            None => root.to_path_buf(),
        }
    };
    for rel in p
        .force_remove
        .iter()
        .chain(&p.remove_files)
        .chain(&p.remove_dirs)
    {
        dirty.insert(parent_of(rel));
    }
    for rel in &p.make_dirs {
        for (i, _) in rel.match_indices('/') {
            dirty.insert(parent_of(&rel[..i]));
        }
        dirty.insert(parent_of(rel));
    }
    for (rel, _) in &p.write_files {
        dirty.insert(parent_of(rel));
    }
    // Group commit (Issue #29, Appendix A.1): write and flush every temp,
    // one barrier, rename each over its target, flush the renamed entries'
    // directories (and every other directory this apply changed), one more
    // barrier; all before the index save. A rename
    // therefore never exposes un-barriered data, and until the index is
    // saved the marker guards everything.
    if !p.write_files.is_empty() {
        use crate::durable;
        let mut temps: Vec<PathBuf> = Vec::with_capacity(p.write_files.len());
        let mut last = None;
        for (_, bytes) in &p.write_files {
            match write_temp(&tmp_dir, bytes) {
                Ok((t, f)) => {
                    temps.push(t);
                    last = Some(f);
                }
                Err(e) => {
                    for t in &temps {
                        let _ = fs::remove_file(t);
                    }
                    return Err(e.into());
                }
            }
        }
        durable::crash_point(durable::FILES_WRITTEN);
        if let Some(f) = last {
            durable::barrier(&f)?;
        }
        durable::crash_point(durable::FILES_BARRIERED);
        for (i, (path, _)) in p.write_files.iter().enumerate() {
            let target = root.join(path);
            if let Err(e) = fs::rename(&temps[i], &target) {
                for t in &temps[i..] {
                    let _ = fs::remove_file(t);
                }
                return Err(e.into());
            }
            if crash {
                std::process::abort();
            }
        }
        durable::crash_point(durable::FILES_RENAMED);
    }
    if !dirty.is_empty() {
        use crate::durable;
        // A dirty dir that no longer exists as a directory (removed, or an
        // ancestor became a file: NotFound / ENOTDIR) is skipped, and its
        // nearest existing ancestor directory is flushed instead (whose
        // entry for the removed/replaced child changed).
        let mut done: BTreeSet<PathBuf> = BTreeSet::new();
        let mut queue: Vec<PathBuf> = dirty.iter().rev().cloned().collect();
        while let Some(d) = queue.pop() {
            if done.contains(&d) {
                continue;
            }
            let gone = match fs::symlink_metadata(&d) {
                Ok(m) => !m.is_dir(),
                Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(20) => {
                    true
                }
                Err(e) => return Err(e.into()),
            };
            if gone {
                done.insert(d.clone());
                if d != root {
                    if let Some(up) = d.parent() {
                        queue.push(up.to_path_buf());
                    }
                }
                continue;
            }
            durable::flush_dir(&d, &fs::File::open(&d)?)?;
            done.insert(d);
        }
        durable::barrier(&fs::File::open(&tmp_dir)?)?;
    }
    let mut idx = Index {
        base: p.base,
        ..Index::default()
    };
    for (path, (node, e, refresh)) in p.index {
        let e = match (e, refresh) {
            (Entry::File { blob, .. }, true) => match fs::metadata(root.join(&path)) {
                Ok(md) => Entry::File {
                    blob,
                    size: md.len(),
                    mtime: mtime_ns(&md),
                },
                // Removed by the user while we applied (e.g. under `hub
                // watch`): keep the entry with unknown stat ("rehash"), so
                // the next scan reports it deleted instead of this apply
                // failing and leaving the marker, whose resume would
                // resurrect the file (#29 repair).
                Err(e) if e.kind() == io::ErrorKind::NotFound => Entry::File {
                    blob,
                    size: 0,
                    mtime: 0,
                },
                Err(e) => return Err(e.into()),
            },
            (e, _) => e,
        };
        idx.entries.insert(path, (node, e));
    }
    idx.save(hub_dir)?;
    if stage.as_deref() == Some(CRASH_AFTER_INDEX) {
        std::process::abort();
    }
    // A crash at any point leaves either the marker (resume re-derives the
    // pin) or the correct pin.
    if let Some(pin) = &p.pin {
        crate::watch::set_pinned(hub_dir, pin.as_ref())?;
    }
    if stage.as_deref() == Some(CRASH_AFTER_PIN) {
        std::process::abort();
    }
    fs::remove_file(&marker)?;
    fs::File::open(hub_dir)?.sync_all()?;
    Ok(warnings)
}

/// The index of target `t` as if it were materialized, without touching
/// the working directory (Issue #49, `reid --rescue`): every file entry has
/// size and mtime 0, so the next scan rehashes it. Case-clashing paths are
/// indexed as unmaterialized (D-019).
pub fn index_for(t: &Target, case_insensitive: bool) -> Index {
    let mut idx = Index {
        base: {
            let mut b = t.frontier.clone();
            b.sort();
            b
        },
        ..Index::default()
    };
    let paths = t.proj.entries.keys().filter(|p| !p.is_empty());
    let clash = clashing(paths, case_insensitive);
    for (path, node) in &t.proj.entries {
        if path.is_empty() {
            continue;
        }
        let e = if clash.contains(path) {
            Entry::Unmaterialized
        } else {
            match t.state[node].kind {
                Kind::File => Entry::File {
                    blob: t
                        .proj
                        .blobs
                        .get(node)
                        .copied()
                        .unwrap_or_else(BlobId::empty),
                    size: 0,
                    mtime: 0,
                },
                _ => Entry::Dir,
            }
        };
        idx.entries.insert(path.clone(), (*node, e));
    }
    idx
}
