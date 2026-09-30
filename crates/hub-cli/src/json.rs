//! `--json` output (Issue #37), schema 1.
//!
//! A tiny hand-rolled JSON value ([`J`]) and writer: no dependency. Strings
//! are escaped per RFC 8259 (`"`, `\`, every control character below
//! U+0020 and U+007F, plus U+2028/U+2029 for JavaScript embedding);
//! non-ASCII is written as UTF-8. Object keys keep insertion order, so
//! documents are byte-for-byte deterministic (golden tests).
//!
//! # Contract (documented in the Wiki page CLI-JSON)
//! - Every document is one line on stdout: an object whose first two keys
//!   are `"schema": 1` and `"command"` (or `"event"` for `watch --json`
//!   lines), and whose last key is `"warnings"` (an array of strings; the
//!   text of each `warning:` line). Nothing else is written on stdout.
//! - Errors: nothing on stdout; one document on stderr,
//!   `{"schema":1,"program":"barocss-hub","error":{"code","message"},"warnings"}`,
//!   and exit status [`EXIT_ERROR`]. `code` is a coarse, stable class
//!   (a [`crate::Code`], chosen where the error is constructed, never
//!   derived from the text); `message` is the human text (not stable).
//! - Exit status: 0 success, [`EXIT_ERROR`] (1) any command error,
//!   [`EXIT_NO_CWD`] (2) the current directory cannot be read.
//! - Ids: node and op ids as `"<counter>.<32 hex replica>"`, version ids
//!   as `{"id": <64 hex>, "short": <display prefix>}`, blob ids as 64 hex.
//!   Paths are root-relative with `/` separators; the root is `""`.
//! - Schema 1 may gain keys; existing keys keep their name and meaning.
//!   A breaking change bumps `schema`.

use crate::Code;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;

use hub_core::checkpoint::{Checkpoint, VersionId, VersionIndex};
use hub_core::projection::{project, Projection};
use hub_core::tree::{evaluate, BlobId, Conflict, Kind, NodeId, Payload, State, ROOT};

use crate::wd::{self, Entry};
use crate::{store, sync, watch, Error, Repo};

/// The flag, accepted anywhere in the arguments.
pub const FLAG: &str = "--json";
pub const SCHEMA: u64 = 1;
/// Exit status of any command error (human and JSON mode).
pub const EXIT_ERROR: u8 = 1;
/// Exit status when the current directory cannot be read.
pub const EXIT_NO_CWD: u8 = 2;

/// Commands with a structured `--json` document. Every other command's
/// document is `{"schema","command","output":[<its human lines>],"warnings"}`.
pub const STRUCTURED: &[&str] = &[
    "help", "status", "log", "warnings", "ls", "show", "cat", "cat-blob", "record", "checkout",
    "commit", "watch", "sync",
];

/// A JSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    Num(u64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub fn s(v: impl Into<String>) -> J {
        J::Str(v.into())
    }
    pub fn n(v: impl TryInto<u64>) -> J {
        J::Num(v.try_into().unwrap_or(u64::MAX))
    }
    pub fn obj(fields: Vec<(&str, J)>) -> J {
        J::Obj(
            fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }
    pub fn opt(v: Option<J>) -> J {
        v.unwrap_or(J::Null)
    }
    pub fn strs<S: AsRef<str>>(v: impl IntoIterator<Item = S>) -> J {
        J::Arr(v.into_iter().map(|x| J::s(x.as_ref())).collect())
    }

    pub fn render(&self) -> String {
        let mut o = String::new();
        self.write(&mut o);
        o
    }

    fn write(&self, o: &mut String) {
        match self {
            J::Null => o.push_str("null"),
            J::Bool(b) => o.push_str(if *b { "true" } else { "false" }),
            J::Num(n) => {
                let _ = write!(o, "{n}");
            }
            J::Str(s) => esc(s, o),
            J::Arr(v) => {
                o.push('[');
                for (i, x) in v.iter().enumerate() {
                    if i > 0 {
                        o.push(',');
                    }
                    x.write(o);
                }
                o.push(']');
            }
            J::Obj(v) => {
                o.push('{');
                for (i, (k, x)) in v.iter().enumerate() {
                    if i > 0 {
                        o.push(',');
                    }
                    esc(k, o);
                    o.push(':');
                    x.write(o);
                }
                o.push('}');
            }
        }
    }
}

/// Write `s` as a JSON string literal.
pub fn esc(s: &str, o: &mut String) {
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            '\u{8}' => o.push_str("\\b"),
            '\u{c}' => o.push_str("\\f"),
            c if (c as u32) < 0x20 || c == '\u{7f}' || c == '\u{2028}' || c == '\u{2029}' => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
}

/// Standard base64 with padding (RFC 4648 §4).
pub fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut o = String::with_capacity(b.len().div_ceil(3) * 4);
    for ch in b.chunks(3) {
        let n = (ch[0] as u32) << 16
            | (*ch.get(1).unwrap_or(&0) as u32) << 8
            | *ch.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= ch.len() {
                o.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                o.push('=');
            }
        }
    }
    o
}

/// A command document: `schema`, `command`, then `fields`.
pub fn doc(command: &str, fields: Vec<(&str, J)>) -> J {
    let mut v = vec![("schema", J::Num(SCHEMA)), ("command", J::s(command))];
    v.extend(fields);
    J::obj(v)
}

/// A `watch --json` event line: `schema`, `event`, then `fields`.
pub fn event(name: &str, fields: Vec<(&str, J)>) -> J {
    let mut v = vec![("schema", J::Num(SCHEMA)), ("event", J::s(name))];
    v.extend(fields);
    J::obj(v)
}

/// Write one document line.
pub fn emit(out: &mut dyn Write, j: J) -> Result<(), Error> {
    let mut s = j.render();
    s.push('\n');
    out.write_all(s.as_bytes())?;
    Ok(())
}

/// The warning texts of captured `warning: ...` lines (other lines kept
/// whole).
pub fn warning_lines(buf: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(buf)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.strip_prefix("warning: ").unwrap_or(l).to_string())
        .collect()
}

/// Append `"warnings"` as the last key of the one-object document `doc`.
pub fn with_warnings(doc: &[u8], warnings: &[String]) -> Vec<u8> {
    let t = String::from_utf8_lossy(doc);
    let t = t.trim_end();
    let w = J::strs(warnings).render();
    let body = match t.strip_suffix('}') {
        Some(b) if b.len() > 1 => format!("{b},\"warnings\":{w}}}\n"),
        Some(_) => format!("{{\"warnings\":{w}}}\n"),
        None => format!("{{\"schema\":{SCHEMA},\"warnings\":{w}}}\n"),
    };
    body.into_bytes()
}

/// The error document written on stderr.
pub fn error_doc(e: &Error, warnings: &[String]) -> J {
    J::obj(vec![
        ("schema", J::Num(SCHEMA)),
        ("program", J::s(crate::PROGRAM)),
        ("error", {
            let mut f = vec![("code", J::s(e.code().as_str())), ("message", J::s(&e.0))];
            if e.is_remote() {
                f.push(("remote", J::Bool(true)));
            }
            J::obj(f)
        }),
        ("warnings", J::strs(warnings)),
    ])
}

/// Run a command without a structured form, wrapping its human lines.
pub(crate) fn captured(
    args: &[String],
    cwd: &Path,
    out: &mut dyn Write,
    warn: &mut dyn Write,
) -> Result<(), Error> {
    let mut buf = Vec::new();
    crate::run(args, cwd, &mut buf, warn)?;
    let text = String::from_utf8_lossy(&buf);
    emit(
        out,
        doc(
            &args[0],
            vec![("output", J::strs(text.lines().collect::<Vec<_>>()))],
        ),
    )
}

// ---------- value helpers ----------

pub fn id(n: NodeId) -> J {
    J::s(store::fmt_id(n))
}

fn rid(r: u128) -> J {
    J::s(format!("{r:032x}"))
}

fn version(ix: &VersionIndex, v: &VersionId) -> J {
    J::obj(vec![("id", J::s(v.hex())), ("short", J::s(ix.short(v)))])
}

fn blob(b: BlobId) -> J {
    J::s(b.hex())
}

/// Every live entry under `under` (excluded), sorted by path: full
/// root-relative paths.
fn entries(state: &State, proj: &Projection, under: NodeId) -> J {
    let base = &proj.paths[&under];
    let pre = if base.is_empty() {
        String::new()
    } else {
        format!("{base}/")
    };
    J::Arr(
        proj.entries
            .iter()
            .filter(|(p, _)| !p.is_empty() && p.starts_with(&pre) && **p != *base)
            .map(|(p, n)| {
                let file = state[n].kind == Kind::File;
                J::obj(vec![
                    ("path", J::s(p)),
                    ("kind", J::s(if file { "file" } else { "dir" })),
                    ("node", id(*n)),
                    (
                        "blob",
                        if file {
                            blob(proj.blobs.get(n).copied().unwrap_or_else(BlobId::empty))
                        } else {
                            J::Null
                        },
                    ),
                ])
            })
            .collect(),
    )
}

fn conflicts(proj: &Projection, cs: &std::collections::BTreeSet<Conflict>) -> J {
    let path = |n: NodeId| J::opt(proj.paths.get(&n).map(J::s));
    let mut v = Vec::new();
    for c in &proj.collisions {
        v.push(J::obj(vec![
            ("kind", J::s("name-collision")),
            ("node", id(c.node)),
            ("path", path(c.node)),
            ("name", J::s(&c.name)),
            ("winner", id(c.winner)),
        ]));
    }
    for c in cs {
        v.push(match c {
            Conflict::InvalidOp { op } => {
                J::obj(vec![("kind", J::s("invalid-op")), ("op", id(*op))])
            }
            Conflict::Cycle {
                winner,
                loser,
                node,
            } => J::obj(vec![
                ("kind", J::s("cycle")),
                ("node", id(*node)),
                ("path", path(*node)),
                ("winner", id(*winner)),
                ("loser", id(*loser)),
            ]),
            Conflict::SameNodeMove {
                winner,
                loser,
                node,
            } => J::obj(vec![
                ("kind", J::s("same-node-move")),
                ("node", id(*node)),
                ("path", path(*node)),
                ("winner", id(*winner)),
                ("loser", id(*loser)),
            ]),
            Conflict::Revived { node, delete, mv } => J::obj(vec![
                ("kind", J::s("revived")),
                ("node", id(*node)),
                ("path", path(*node)),
                ("delete", id(*delete)),
                ("move", id(*mv)),
            ]),
            Conflict::Content {
                node,
                winner,
                loser,
                winner_blob,
                loser_blob,
            } => J::obj(vec![
                ("kind", J::s("content")),
                ("node", id(*node)),
                ("path", path(*node)),
                ("winner", id(*winner)),
                ("loser", id(*loser)),
                ("winner_blob", blob(*winner_blob)),
                ("loser_blob", blob(*loser_blob)),
            ]),
            Conflict::EditRevived { node, edit, delete } => J::obj(vec![
                ("kind", J::s("edit-revived")),
                ("node", id(*node)),
                ("path", path(*node)),
                ("edit", id(*edit)),
                ("delete", id(*delete)),
            ]),
        });
    }
    J::Arr(v)
}

fn change(c: &crate::ChangeInfo) -> J {
    J::obj(vec![
        ("kind", J::s(c.kind)),
        ("path", J::s(&c.path)),
        ("from", J::opt(c.from.as_ref().map(J::s))),
        ("node", J::opt(c.node.map(id))),
        ("dir", J::Bool(c.dir)),
    ])
}

pub(crate) fn changes(cs: &[crate::ChangeInfo]) -> J {
    J::Arr(cs.iter().map(change).collect())
}

fn checkpoint(ix: &VersionIndex, c: &Checkpoint, published: bool) -> Vec<(&'static str, J)> {
    vec![
        ("version", version(ix, &c.id())),
        ("message", J::s(&c.message)),
        ("published", J::Bool(published)),
        ("author", rid(c.author.0)),
    ]
}

/// Local then published-only checkpoints, newest first (as `log`).
fn log_list(repo: &Repo) -> Result<Vec<(Checkpoint, bool)>, Error> {
    let published = sync::read_published(repo)?;
    let mut list: Vec<&Checkpoint> = repo.checkpoints.iter().collect();
    for c in &published {
        if !list.contains(&c) {
            list.push(c);
        }
    }
    Ok(list
        .into_iter()
        .rev()
        .map(|c| (c.clone(), published.contains(c)))
        .collect())
}

// ---------- read commands ----------

/// `status`, `log`, `warnings`, `ls`, `show` under `--json`.
pub(crate) fn read_cmd(
    repo: &Repo,
    state: &State,
    proj: &Projection,
    a: &[&str],
    out: &mut dyn Write,
    warn: &mut dyn Write,
) -> Result<(), Error> {
    let usage = |c: &str| {
        crate::errc::<()>(
            crate::Code::Usage,
            format!("usage: barocss-hub {c}  (see barocss-hub --help)"),
        )
    };
    let d = match a {
        ["ls"] | ["ls", "-l"] => doc(
            "ls",
            vec![("path", J::s("")), ("entries", entries(state, proj, ROOT))],
        ),
        ["ls", p] | ["ls", "-l", p] => {
            let n = crate::lookup(proj, p)?;
            doc(
                "ls",
                vec![
                    ("path", J::s(&proj.paths[&n])),
                    ("entries", entries(state, proj, n)),
                ],
            )
        }
        ["show", v] | ["show", "-l", v] => show_doc(repo, v)?,
        ["status"] | ["status", "--ignored"] => status_doc(repo, proj, warn)?,
        ["log"] => {
            let ix = repo.index();
            let list = log_list(repo)?
                .iter()
                .map(|(c, p)| J::obj(checkpoint(&ix, c, *p)))
                .collect();
            doc("log", vec![("checkpoints", J::Arr(list))])
        }
        ["log", "--path", p] => path_history_doc(repo, p)?,
        ["warnings"] => {
            let (sw, _) = sync::warnings_view(repo)?;
            let pend = sync::read_pending(&repo.dir)?;
            doc(
                "warnings",
                vec![
                    ("sync_warnings", sync_warnings(&sw)),
                    (
                        "pending",
                        J::Arr(
                            pend.iter()
                                .map(|(o, p)| J::obj(vec![("op", id(o.id)), ("peer", rid(p.0))]))
                                .collect(),
                        ),
                    ),
                ],
            )
        }
        [c, ..] => return usage(c),
        [] => return usage("help"),
    };
    emit(out, d)
}

fn sync_warnings(sw: &[hub_core::sync::RejectWarning]) -> J {
    J::Arr(
        sw.iter()
            .map(|w| {
                let r = sync::render_warning(w);
                let sides = sync::equivocation_sides(w);
                J::obj(vec![
                    ("reason", J::s(sync::reason_str(&w.reason))),
                    ("peer", rid(w.peer.0)),
                    ("author", J::opt(w.author.map(|a| rid(a.0)))),
                    ("op", J::opt(w.rejected.map(id))),
                    ("detail", J::s(&w.detail)),
                    ("kept", J::opt(sides.map(|(k, _)| J::s(k)))),
                    ("received", J::opt(sides.map(|(_, r)| J::s(r)))),
                    ("action", J::s(sync::warning_action(w))),
                    ("stuck", J::Arr(w.stuck.iter().map(|o| id(*o)).collect())),
                    ("first_seen", J::n(w.first_seen)),
                    ("text", J::s(r)),
                ])
            })
            .collect(),
    )
}

fn show_doc(repo: &Repo, v: &str) -> Result<J, Error> {
    let ix = repo.index();
    let ver = crate::resolve(repo, &ix, v)?;
    let vid = hub_core::checkpoint::version_id(&repo.store, &ver);
    let st = evaluate(ver.resolve(&repo.store)).state;
    let p = project(&st);
    let msgs: Vec<&str> = repo
        .checkpoints
        .iter()
        .filter(|c| c.id() == vid)
        .map(|c| c.message.as_str())
        .collect();
    Ok(doc(
        "show",
        vec![
            ("version", version(&ix, &vid)),
            ("messages", J::strs(msgs)),
            ("entries", entries(&st, &p, ROOT)),
        ],
    ))
}

fn status_doc(repo: &Repo, proj: &Projection, warn: &mut dyn Write) -> Result<J, Error> {
    let ix = repo.index();
    let head_f = repo.head_frontier();
    let hid = VersionId::of_frontier(&head_f);
    let published = sync::read_published(repo)?.len();
    let (sw, npend) = sync::warnings_view(repo)?;
    let remotes = crate::remote::remotes_view(&repo.dir)?;
    let remotes_j = J::Arr(
        remotes
            .iter()
            .map(|r| {
                J::obj(vec![
                    ("name", J::s(&r.name)),
                    ("url", J::s(&r.url)),
                    ("default", J::Bool(r.default)),
                ])
            })
            .collect(),
    );
    if crate::remote::is_bare(&repo.dir) {
        // D-042: a bare repository has no working-directory section.
        return Ok(doc(
            "status",
            vec![
                ("root", J::s(repo.root().to_string_lossy())),
                ("bare", J::Bool(true)),
                (
                    "head",
                    J::obj(vec![
                        ("version", version(&ix, &hid)),
                        ("ops", J::n(repo.store.len())),
                        ("checkpoints", J::n(repo.checkpoints.len())),
                        ("published", J::n(published)),
                    ]),
                ),
                ("conflicts", conflicts(proj, &repo.conflicts())),
                ("sync_warnings", sync_warnings(&sw)),
                ("pending_ops", J::n(npend)),
                ("remotes", remotes_j),
            ],
        ));
    }
    let pinned = watch::pinned(&repo.dir)?;
    let active = watch::active_watch(&repo.dir)?;
    let interrupted = wd::interrupted(&repo.dir)?.is_some();
    let idx = crate::load_index(repo, warn)?;
    let behind = if idx.base == head_f {
        0
    } else {
        sync::behind_by(repo, &idx.base)
    };
    let state = if pinned.is_some() {
        "pinned"
    } else if idx.base == head_f {
        "at-head"
    } else if behind > 0 {
        "behind"
    } else {
        "unknown"
    };
    let mut ws = Vec::new();
    let (mut w, ignored) = wd::scan_all(&repo.root(), &idx, &mut ws)?;
    w.retain(|p, _| !ignored.contains(p));
    crate::print_warnings(warn, &ws)?;
    let ch = wd::diff(&idx, &w);
    let infos = crate::change_infos(&ch, &idx, &w, &BTreeMap::new());
    let tbi = wd::tracked_but_ignored(&repo.root(), &idx)?;
    let ign: Vec<J> = ignored
        .iter()
        .filter(|p| !p.match_indices('/').any(|(i, _)| ignored.contains(&p[..i])))
        .map(|p| {
            let dir = crate::w_all_is_dir(&repo.root(), p);
            J::obj(vec![
                ("path", J::s(p)),
                ("dir", J::Bool(dir)),
                (
                    "default",
                    J::Bool(crate::ignore::Ignore::is_default_ignored(
                        &repo.root(),
                        p,
                        dir,
                    )),
                ),
            ])
        })
        .collect();
    let hidden = ign
        .iter()
        .filter(|j| matches!(j, J::Obj(f) if f.iter().any(|(k, v)| k == "default" && *v == J::Bool(true))))
        .count();
    let unmat: Vec<&String> = idx
        .entries
        .iter()
        .filter(|(_, (_, e))| *e == Entry::Unmaterialized)
        .map(|(p, _)| p)
        .collect();
    let blocked = wd::blocked(&idx, &w);
    let clean = ch.is_empty() && blocked.is_empty();
    let pid = active.as_ref().and_then(|p| p.parse::<u64>().ok());
    Ok(doc(
        "status",
        vec![
            ("root", J::s(repo.root().to_string_lossy())),
            (
                "head",
                J::obj(vec![
                    ("version", version(&ix, &hid)),
                    ("ops", J::n(repo.store.len())),
                    ("checkpoints", J::n(repo.checkpoints.len())),
                    ("published", J::n(published)),
                ]),
            ),
            (
                "workdir",
                J::obj(vec![
                    ("state", J::s(state)),
                    ("version", version(&ix, &VersionId::of_frontier(&idx.base))),
                    ("behind_ops", J::n(behind)),
                    (
                        "pinned",
                        J::opt(pinned.map(|p| version(&ix, &VersionId::of_frontier(&p)))),
                    ),
                    ("interrupted_checkout", J::Bool(interrupted)),
                    ("clean", J::Bool(clean)),
                ]),
            ),
            (
                "watch",
                J::obj(vec![
                    ("running", J::Bool(active.is_some())),
                    ("pid", J::opt(pid.map(J::Num))),
                ]),
            ),
            ("conflicts", conflicts(proj, &repo.conflicts())),
            ("sync_warnings", sync_warnings(&sw)),
            ("pending_ops", J::n(npend)),
            ("remotes", remotes_j),
            ("changes", changes(&infos)),
            (
                "blocked",
                J::Arr(
                    blocked
                        .iter()
                        .map(|b| {
                            J::obj(vec![
                                ("path", J::s(&b.path)),
                                (
                                    "with",
                                    J::Arr(
                                        b.with
                                            .iter()
                                            .map(|(p, n)| {
                                                J::obj(vec![("path", J::s(p)), ("node", id(*n))])
                                            })
                                            .collect(),
                                    ),
                                ),
                            ])
                        })
                        .collect(),
                ),
            ),
            ("unmaterialized", J::strs(unmat)),
            ("tracked_but_ignored", J::strs(tbi)),
            ("ignored", J::Arr(ign)),
            ("hidden_by_defaults", J::n(hidden)),
        ],
    ))
}

// ---------- per-file history (Timeline) ----------

struct History {
    node: NodeId,
    path: String,
    /// Checkpoints (newest first) in which the node is live: (checkpoint,
    /// published, its path there, its blob there or `None` for a dir).
    checkpoints: Vec<(Checkpoint, bool, String, Option<BlobId>)>,
}

fn path_history(repo: &Repo, p: &str) -> Result<History, Error> {
    let proj = project(repo.state());
    let node = crate::lookup(&proj, p)?;
    if node == ROOT {
        return crate::errc(
            Code::Usage,
            "usage: barocss-hub log [--path <path>]  (see barocss-hub --help)",
        );
    }
    let mut memo: BTreeMap<VersionId, Option<(String, Option<BlobId>)>> = BTreeMap::new();
    let mut out = Vec::new();
    for (c, published) in log_list(repo)? {
        let at = memo
            .entry(c.id())
            .or_insert_with(|| {
                let v = c.version(&repo.store).ok()?;
                let st = evaluate(v.resolve(&repo.store)).state;
                let pj = project(&st);
                let path = pj.paths.get(&node)?.clone();
                let b = (st[&node].kind == Kind::File)
                    .then(|| pj.blobs.get(&node).copied().unwrap_or_else(BlobId::empty));
                Some((path, b))
            })
            .clone();
        if let Some((path, b)) = at {
            out.push((c, published, path, b));
        }
    }
    Ok(History {
        node,
        path: proj.paths[&node].clone(),
        checkpoints: out,
    })
}

/// Human `log --path <p>`: `<short> <blob12|-> <path then> <message>`.
pub(crate) fn path_history_lines(repo: &Repo, p: &str) -> Result<Vec<String>, Error> {
    let h = path_history(repo, p)?;
    let ix = repo.index();
    Ok(h.checkpoints
        .iter()
        .map(|(c, published, path, b)| {
            format!(
                "{} {} {path}{} {}",
                ix.short(&c.id()),
                b.map(|b| b.hex()[..12].to_string())
                    .unwrap_or_else(|| "-".into()),
                if *published { " [published]" } else { "" },
                c.message
            )
        })
        .collect())
}

fn path_history_doc(repo: &Repo, p: &str) -> Result<J, Error> {
    let h = path_history(repo, p)?;
    let ix = repo.index();
    let cks = h
        .checkpoints
        .iter()
        .map(|(c, published, path, b)| {
            let mut f = checkpoint(&ix, c, *published);
            f.push(("path", J::s(path)));
            f.push(("blob", J::opt(b.map(blob))));
            J::obj(f)
        })
        .collect();
    // Every op on the node, newest first (store order is a topological
    // order: deps have smaller Lamport counters).
    let ops = repo
        .store
        .values()
        .rev()
        .filter_map(|o| {
            let f = match &o.payload {
                Payload::Create { parent, name, .. } if o.id == h.node => vec![
                    ("kind", J::s("create")),
                    ("parent", id(*parent)),
                    ("name", J::s(name)),
                ],
                Payload::Move {
                    node,
                    new_parent,
                    new_name,
                } if *node == h.node => vec![
                    ("kind", J::s("move")),
                    ("parent", id(*new_parent)),
                    ("name", J::s(new_name)),
                ],
                Payload::SetContent { node, blob: b } if *node == h.node => {
                    vec![("kind", J::s("set-content")), ("blob", blob(*b))]
                }
                Payload::Delete { node } if *node == h.node => vec![("kind", J::s("delete"))],
                _ => return None,
            };
            let mut v = vec![("op", id(o.id))];
            v.extend(f);
            Some(J::obj(v))
        })
        .collect();
    let (kind, cur) = match repo.state()[&h.node].kind {
        Kind::File => (
            "file",
            blob(
                project(repo.state())
                    .blobs
                    .get(&h.node)
                    .copied()
                    .unwrap_or_else(BlobId::empty),
            ),
        ),
        _ => ("dir", J::Null),
    };
    Ok(doc(
        "log",
        vec![
            ("path", J::s(&h.path)),
            ("node", id(h.node)),
            ("kind", J::s(kind)),
            ("blob", cur),
            ("checkpoints", J::Arr(cks)),
            ("ops", J::Arr(ops)),
        ],
    ))
}

// ---------- writers and bytes ----------

pub(crate) fn record_doc(repo: &Repo, r: Option<&crate::Recorded>) -> J {
    let ix = repo.index();
    let hid = VersionId::of_frontier(&repo.head_frontier());
    doc(
        "record",
        vec![
            ("recorded", J::Bool(r.is_some())),
            ("changes", changes(r.map(|r| &r.changes[..]).unwrap_or(&[]))),
            ("head", version(&ix, &hid)),
            (
                "checkpoint",
                J::opt(
                    r.and_then(|r| r.checkpoint.as_ref())
                        .map(|c| J::obj(checkpoint(&ix, c, false))),
                ),
            ),
        ],
    )
}

/// Counts object of a sync (`{ops, blobs, checkpoints}`).
pub fn counts_j(c: &crate::sync::Counts) -> J {
    J::obj(vec![
        ("ops", J::n(c.ops)),
        ("blobs", J::n(c.blobs)),
        ("checkpoints", J::n(c.checkpoints)),
    ])
}

/// Rejected ops as `[{"op","reason","message"}]` (Issue #45).
pub fn rejected_j(rej: &[(hub_core::tree::OpId, String)]) -> J {
    J::Arr(
        rej.iter()
            .map(|(o, why)| {
                J::obj(vec![
                    ("op", id(*o)),
                    ("reason", J::s(why)),
                    ("message", J::s(crate::sync::rejected_message(why))),
                ])
            })
            .collect(),
    )
}

/// `sync --json` (Issue #43): counts, `read_only`, head, and the human
/// lines as `output`.
pub fn sync_doc(r: &crate::sync::Synced, warn: &mut dyn Write) -> Result<J, Error> {
    let mut o = Vec::new();
    crate::sync::print_report(r, &mut o, warn)?;
    let text = String::from_utf8_lossy(&o).into_owned();
    Ok(doc(
        "sync",
        vec![
            ("remote", J::s(&r.remote)),
            ("received", counts_j(&r.report.received)),
            ("sent", counts_j(&r.report.sent)),
            ("read_only", J::Bool(r.report.read_only)),
            (
                "head",
                J::obj(vec![
                    ("id", J::s(&r.head_id)),
                    ("short", J::s(&r.head_short)),
                ]),
            ),
            ("pending_ops", J::n(r.pending)),
            ("rejected", rejected_j(&r.report.rejected)),
            ("rejected_reported", J::Bool(r.report.rejected_reported)),
            ("output", J::strs(text.lines().collect::<Vec<_>>())),
        ],
    ))
}

pub(crate) fn checkout_fields(c: &crate::CheckedOut) -> Vec<(&'static str, J)> {
    vec![
        (
            "version",
            J::obj(vec![
                ("id", J::s(c.version.hex())),
                ("short", J::s(&c.short)),
            ]),
        ),
        ("written", J::n(c.written)),
        ("removed", J::n(c.removed)),
        ("at_head", J::Bool(c.at_head)),
    ]
}

pub(crate) fn checkout_doc(c: &crate::CheckedOut) -> J {
    doc("checkout", checkout_fields(c))
}

pub(crate) fn commit_doc(repo: &Repo, ix: &VersionIndex, c: &Checkpoint) -> J {
    let published = sync::read_published(repo)
        .map(|p| p.contains(c))
        .unwrap_or(false);
    doc(
        "commit",
        vec![("checkpoint", J::obj(checkpoint(ix, c, published)))],
    )
}

/// Resolve `cat <version> <path>` to (version id, node, blob).
pub(crate) fn cat_target(
    repo: &Repo,
    v: &str,
    p: &str,
) -> Result<(VersionId, NodeId, BlobId), Error> {
    let ix = repo.index();
    let ver = crate::resolve(repo, &ix, v)?;
    let vid = hub_core::checkpoint::version_id(&repo.store, &ver);
    let st = evaluate(ver.resolve(&repo.store)).state;
    let pj = project(&st);
    let node = crate::lookup(&pj, p)?;
    if st[&node].kind != Kind::File {
        return crate::errc(Code::NotAFile, format!("not a file: '{p}'"));
    }
    Ok((
        vid,
        node,
        pj.blobs.get(&node).copied().unwrap_or_else(BlobId::empty),
    ))
}

pub(crate) fn parse_blob(t: &str) -> Result<BlobId, Error> {
    if t.len() != 64 || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return crate::errc(
            Code::BadBlobId,
            format!("invalid blob id '{t}' (need 64 hex digits)"),
        );
    }
    store::parse_blob(&t.to_ascii_lowercase())
        .map_err(|e| Error::new(Code::BadBlobId, format!("invalid blob id '{t}': {e}")))
}

fn content(bytes: &[u8]) -> Vec<(&'static str, J)> {
    vec![
        ("size", J::n(bytes.len())),
        ("encoding", J::s("base64")),
        ("content", J::s(base64(bytes))),
    ]
}

pub(crate) fn cat_doc(
    v: &VersionId,
    short: &str,
    p: &str,
    node: NodeId,
    b: BlobId,
    bytes: &[u8],
) -> J {
    let mut f = vec![
        (
            "version",
            J::obj(vec![("id", J::s(v.hex())), ("short", J::s(short))]),
        ),
        ("path", J::s(p.trim_matches('/'))),
        ("node", id(node)),
        ("blob", blob(b)),
    ];
    f.extend(content(bytes));
    doc("cat", f)
}

pub(crate) fn blob_doc(b: BlobId, bytes: &[u8]) -> J {
    let mut f = vec![("blob", blob(b))];
    f.extend(content(bytes));
    doc("cat-blob", f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_control_chars_quotes_and_keeps_non_ascii() {
        let all: String = (0u32..0x20).filter_map(char::from_u32).collect();
        let j = J::s(format!("{all}\"\\\u{7f}é한🦀\u{2028}\u{2029}/"));
        assert_eq!(
            j.render(),
            "\"\\u0000\\u0001\\u0002\\u0003\\u0004\\u0005\\u0006\\u0007\\b\\t\\n\\u000b\\f\\r\
             \\u000e\\u000f\\u0010\\u0011\\u0012\\u0013\\u0014\\u0015\\u0016\\u0017\\u0018\
             \\u0019\\u001a\\u001b\\u001c\\u001d\\u001e\\u001f\\\"\\\\\\u007fé한🦀\\u2028\\u2029/\""
        );
        // Keys are escaped too; structure and order are kept.
        let o = J::obj(vec![
            ("a\"b", J::Null),
            ("n", J::Num(u64::MAX)),
            ("t", J::Bool(true)),
            (
                "x",
                J::Arr(vec![J::Bool(false), J::Arr(vec![]), J::Obj(vec![])]),
            ),
        ]);
        assert_eq!(
            o.render(),
            "{\"a\\\"b\":null,\"n\":18446744073709551615,\"t\":true,\"x\":[false,[],{}]}"
        );
    }

    #[test]
    fn base64_rfc4648_vectors() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
        }
        assert_eq!(base64(&[0xff, 0xfe, 0x00]), "//4A");
    }

    #[test]
    fn warnings_are_folded_as_the_last_key() {
        let d = with_warnings(b"{\"schema\":1,\"command\":\"ls\"}\n", &["a\"b".into()]);
        assert_eq!(
            String::from_utf8(d).unwrap(),
            "{\"schema\":1,\"command\":\"ls\",\"warnings\":[\"a\\\"b\"]}\n"
        );
        assert_eq!(
            warning_lines(b"warning: x\nwarning: y\nplain\n"),
            vec!["x", "y", "plain"]
        );
    }

    #[test]
    fn code_names_are_unique_and_kebab() {
        let names: Vec<&str> = crate::Code::ALL.iter().map(|c| c.as_str()).collect();
        let mut u = names.clone();
        u.sort();
        u.dedup();
        assert_eq!(u.len(), names.len());
        assert!(names
            .iter()
            .all(|n| n.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')));
    }
}
