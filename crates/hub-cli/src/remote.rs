//! Remotes, URLs, the SSH transport and `serve` (Issue #43, Wiki
//! Remote-Sync §5.3, §5.5, §5.6, §5.7, §5.9).
//!
//! # `.hub/remotes` (local config, never synced)
//! Header `barocss-hub/remotes/v0`, then one line per remote:
//! `R s<name> s<url> [exec=<path>] [default=1]`. Written with
//! temp + fsync + rename + directory sync under the exclusive repo lock.
//!
//! # peers → remotes (D-041)
//! The first command that takes the exclusive lock and finds `.hub/peers`
//! converts every peer root to a `file://` remote (named after the root's
//! basename if valid and unused, else `local-<n>`), deduplicated by URL,
//! writes `remotes` atomically, and only then removes `peers`. A crash
//! before the rename leaves both files as they were; after it, the rerun
//! merges idempotently and removes `peers`. Shared-lock commands read both.
//!
//! # URLs
//! `ssh://[user@]host[:port]/abs/path` (`/~/p` = home-relative), scp-like
//! `[user@]host:path` (relative = home-relative, sent as `~/path`),
//! `file:///abs/path` or a local path; `http(s)://` is `unsupported`.
//!
//! # `serve --stdio`
//! Serves exactly the repository named on its command line (no frame
//! selects a repo). With `--from-ssh-original-command` the path comes from
//! `SSH_ORIGINAL_COMMAND`, parsed with the exact §5.9.6 grammar (no shell),
//! and the environment is ignored for root and exec. Paths are confined
//! to `--root` (else `BAROCSS_HUB_SERVE_ROOT`, not under a forced command).

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use hub_core::tree::valid_name;

use crate::session::{self, Access, Side};
use crate::store::{self, LockMode, RepoLock};
use crate::sync::{self, find_hub, Synced};
use crate::wire::Conn;
use crate::{durable, errc, watch, Code, Error, Repo};

pub const REMOTES_HEADER: &str = "barocss-hub/remotes/v0";
pub const BARE_FILE: &str = "bare";

/// Crash points of the peers → remotes migration (tests).
pub const RMIG_TMP: &str = "rmig-tmp";
pub const RMIG_RENAMED: &str = "rmig-renamed";
pub const RMIG_PEERS_REMOVED: &str = "rmig-peers-removed";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub url: String,
    pub exec: Option<String>,
    pub default: bool,
}

// ------------------------------------------------------------ remotes file

pub fn read_remotes(hub: &Path) -> Result<Vec<Remote>, Error> {
    let rs = read_remotes_unchecked(hub)?;
    let p = hub.join("remotes");
    for r in &rs {
        if let Err(why) = check_remote(r) {
            return errc(
                Code::Corrupt,
                format!(
                    "refusing {}: remote {:?} is unsafe: {why}; nothing was run. \
                     Fix the line, or remove it with `barocss-hub remote rm {}`",
                    p.display(),
                    r.name,
                    if remote_name_ok(&r.name) {
                        r.name.as_str()
                    } else {
                        "<name>"
                    }
                ),
            );
        }
    }
    Ok(rs)
}

/// Every stored field is re-validated at load (Issue #45): a hand-edited or
/// planted `.hub/remotes` can never feed an unchecked exec, host, user or
/// path to ssh.
pub fn check_remote(r: &Remote) -> Result<(), String> {
    if !remote_name_ok(&r.name) {
        return Err(format!("invalid remote name {:?}", r.name));
    }
    if let Some(e) = &r.exec {
        exec_check(e)?;
    }
    match parse_url(&r.url, Path::new("/")) {
        Ok(Url::Ssh { .. }) => {}
        Ok(Url::File(_)) if r.url.starts_with("file:///") => {}
        Ok(Url::File(_)) => return Err(format!("url {:?} is not file:///<abs> or ssh", r.url)),
        Err(e) => return Err(e.0),
    }
    Ok(())
}

/// The file as written, without the safety check (only `remote rm` uses
/// it, so an unsafe line can be removed).
fn read_remotes_unchecked(hub: &Path) -> Result<Vec<Remote>, Error> {
    let p = hub.join("remotes");
    let text = match fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let bad = || Error::new(Code::Corrupt, format!("corrupt {}", p.display()));
    let mut lines = text.lines();
    if lines.next() != Some(REMOTES_HEADER) {
        return Err(bad());
    }
    let mut out = Vec::new();
    for l in lines.filter(|l| !l.is_empty()) {
        let t: Vec<&str> = l.split(' ').collect();
        let ["R", name, url, rest @ ..] = t.as_slice() else {
            return Err(bad());
        };
        let mut r = Remote {
            name: store::unesc(name).map_err(|_| bad())?,
            url: store::unesc(url).map_err(|_| bad())?,
            exec: None,
            default: false,
        };
        for kv in rest {
            match kv.split_once('=') {
                Some(("exec", v)) => r.exec = Some(v.to_string()),
                Some(("default", "1")) => r.default = true,
                _ => {} // unknown keys are kept out; later versions may add some
            }
        }
        out.push(r);
    }
    Ok(out)
}

fn encode_remotes(rs: &[Remote]) -> String {
    let mut s = format!("{REMOTES_HEADER}\n");
    for r in rs {
        s.push_str(&format!("R {} {}", store::esc(&r.name), store::esc(&r.url)));
        if let Some(e) = &r.exec {
            s.push_str(&format!(" exec={e}"));
        }
        if r.default {
            s.push_str(" default=1");
        }
        s.push('\n');
    }
    s
}

fn dsync(dir: &Path) -> Result<(), Error> {
    let d = File::open(dir)?;
    durable::flush_dir(dir, &d)?;
    durable::barrier(&d)?;
    Ok(())
}

/// Temp + fsync + rename + dir sync, with crash points (migration tests).
fn write_remotes(hub: &Path, rs: &[Remote], migrating: bool) -> Result<(), Error> {
    let tmp = hub.join(".remotes.tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(encode_remotes(rs).as_bytes())?;
        durable::flush(&f)?;
        durable::barrier(&f)?;
    }
    if migrating {
        durable::crash_point(RMIG_TMP);
    }
    fs::rename(&tmp, hub.join("remotes"))?;
    dsync(hub)?;
    if migrating {
        durable::crash_point(RMIG_RENAMED);
    }
    Ok(())
}

fn file_url(root: &Path) -> String {
    format!("file://{}", root.display())
}

/// `rs` plus the peers of a not-yet-migrated `.hub/peers` (dedup by URL).
fn merged(hub: &Path, mut rs: Vec<Remote>) -> Result<(Vec<Remote>, bool), Error> {
    let peers = watch::read_peers(hub)?;
    let mut changed = false;
    for (i, p) in peers.iter().enumerate() {
        let url = file_url(p);
        if rs.iter().any(|r| r.url == url) {
            continue;
        }
        let base = p
            .file_name()
            .map(|b| b.to_string_lossy().into_owned())
            .unwrap_or_default();
        let used = |n: &str, rs: &[Remote]| rs.iter().any(|r| r.name == n);
        let name = if remote_name_ok(&base) && !used(&base, &rs) {
            base
        } else {
            let mut n = i + 1;
            while used(&format!("local-{n}"), &rs) {
                n += 1;
            }
            format!("local-{n}")
        };
        let default = rs.is_empty();
        rs.push(Remote {
            name,
            url,
            exec: None,
            default,
        });
        changed = true;
    }
    Ok((rs, changed))
}

/// Remotes as a shared-lock reader sees them: `remotes` plus any peers
/// still waiting for migration (nothing is written).
pub fn remotes_view(hub: &Path) -> Result<Vec<Remote>, Error> {
    Ok(merged(hub, read_remotes(hub)?)?.0)
}

/// The peers → remotes migration. Caller holds the exclusive repo lock.
pub fn migrate_peers(hub: &Path) -> Result<(), Error> {
    let peers = hub.join("peers");
    if !peers.exists() {
        return Ok(());
    }
    let (rs, changed) = merged(hub, read_remotes_unchecked(hub)?)?;
    if changed {
        write_remotes(hub, &rs, true)?;
    }
    fs::remove_file(&peers)?;
    dsync(hub)?;
    durable::crash_point(RMIG_PEERS_REMOVED);
    Ok(())
}

pub fn remote_name_ok(n: &str) -> bool {
    valid_name(n) && !n.bytes().any(|b| b <= 0x20 || b == 0x7f)
}

/// The remote program (`exec=`, Issue #45): one word of
/// `[A-Za-z0-9._/~+-]`, not starting with `-`, at most 1024 bytes. It is
/// the first word of the remote shell command, so nothing in it may be
/// special to any shell (no space, quote, `;`, `$`, backquote, glob, ...).
pub fn exec_check(e: &str) -> Result<(), String> {
    if e.is_empty() {
        return Err("exec is empty".into());
    }
    if e.len() > 1024 {
        return Err("exec is longer than 1024 bytes".into());
    }
    if e.starts_with('-') {
        return Err(format!("exec {e:?} starts with '-'"));
    }
    if let Some(c) = e
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '~' | '+' | '-')))
    {
        return Err(format!(
            "exec {e:?} contains {c:?}; only letters, digits and . _ / ~ + - are allowed"
        ));
    }
    Ok(())
}

/// `barocss-hub remote add|list|rm|set-url`.
pub fn remote_cmd(cwd: &Path, args: &[&str], out: &mut dyn Write) -> Result<(), Error> {
    let hub = fs::canonicalize(find_hub(cwd)?)?;
    let usage = || {
        Error::new(
            Code::Usage,
            "usage: barocss-hub remote add [--default] [--exec <path>] [<name>] <url> | list | rm <name> | set-url <name> <url> | trust <name>  (see barocss-hub --help)",
        )
    };
    if args == ["list"] {
        let _l = RepoLock::acquire(&hub, LockMode::Shared)?;
        for r in remotes_view(&hub)? {
            writeln!(
                out,
                "{}\t{}{}",
                r.name,
                r.url,
                if r.default { "\t(default)" } else { "" }
            )?;
        }
        return Ok(());
    }
    let _l = RepoLock::acquire(&hub, LockMode::Exclusive)?;
    migrate_peers(&hub)?;
    // `rm` must work on a file that fails the safety check (to repair it).
    let mut rs = if matches!(args, ["rm", _]) {
        read_remotes_unchecked(&hub)?
    } else {
        read_remotes(&hub)?
    };
    match args {
        ["add", rest @ ..] => {
            let mut default = false;
            let mut exec = None;
            let mut pos = Vec::new();
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match *a {
                    "--default" => default = true,
                    "--exec" => exec = Some(it.next().ok_or_else(usage)?.to_string()),
                    _ => pos.push(*a),
                }
            }
            let (name, url) = match pos.as_slice() {
                [u] => ("origin", *u),
                [n, u] => (*n, *u),
                _ => return Err(usage()),
            };
            if !remote_name_ok(name) {
                return errc(Code::InvalidPath, format!("invalid remote name {name:?}"));
            }
            if let Some(e) = &exec {
                if let Err(why) = exec_check(e) {
                    return errc(Code::Usage, format!("bad --exec: {why}"));
                }
            }
            if rs.iter().any(|r| r.name == name) {
                return errc(Code::AlreadyExists, format!("remote {name} already exists"));
            }
            let url = canonical_url(cwd, url)?;
            if let Some(p) = url.strip_prefix("file://") {
                let p = Path::new(p);
                if !p.join(crate::HUB_DIR).is_dir() {
                    return errc(
                        Code::NotARepository,
                        format!(
                            "no hub repository at {} (run `barocss-hub init` there)",
                            pos[pos.len() - 1]
                        ),
                    );
                }
                if hub.parent() == Some(p) {
                    return errc(Code::Usage, "cannot add this repository as its own remote");
                }
            }
            if rs.iter().any(|r| r.url == url) {
                return errc(Code::AlreadyExists, format!("already a remote: {url}"));
            }
            if default {
                for r in &mut rs {
                    r.default = false;
                }
            }
            let default = default || rs.is_empty();
            let r = Remote {
                name: name.into(),
                url: url.clone(),
                exec,
                default,
            };
            rs.push(r.clone());
            write_remotes(&hub, &rs, false)?;
            writeln!(out, "added remote {name} {url}")?;
            trust_added(&r, &hub, out)?;
        }
        ["rm", name] => {
            let before = rs.len();
            let was_default = rs.iter().any(|r| r.name == *name && r.default);
            rs.retain(|r| r.name != *name);
            if rs.len() == before {
                return errc(Code::NotFound, format!("no remote named {name}"));
            }
            if was_default {
                if let Some(r) = rs.first_mut() {
                    r.default = true;
                }
            }
            write_remotes(&hub, &rs, false)?;
            writeln!(out, "removed remote {name}")?;
        }
        ["set-url", name, url] => {
            let url = canonical_url(cwd, url)?;
            let Some(r) = rs.iter_mut().find(|r| r.name == *name) else {
                return errc(Code::NotFound, format!("no remote named {name}"));
            };
            r.url = url.clone();
            let r = r.clone();
            write_remotes(&hub, &rs, false)?;
            writeln!(out, "remote {name} is now {url}")?;
            trust_added(&r, &hub, out)?;
        }
        ["trust", name] => {
            let Some(r) = rs.iter().find(|r| r.name == *name) else {
                return errc(Code::NotFound, format!("no remote named {name}"));
            };
            let key = trust_key(r, &hub)?;
            crate::trust::add(&key)?;
            writeln!(out, "trusted remote {name} ({})", key.describe())?;
        }
        _ => return Err(usage()),
    }
    Ok(())
}

// ------------------------------------------------------------ URLs

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Url {
    File(PathBuf),
    Ssh {
        user: Option<String>,
        host: String,
        port: Option<u16>,
        /// Absolute, or `~/…` (home-relative).
        path: String,
    },
}

/// A user name: `[A-Za-z0-9._-]`, not starting with `-` (Issue #45).
fn user_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A host: a DNS name or IPv4 (`[A-Za-z0-9._-]`, not starting with `-`
/// or `.`), or a bracketed IPv6 literal `[hex:...[%zone]]`. Nothing that
/// ssh's `%h` expansion could hand to a `ProxyCommand` shell as syntax
/// (Issue #45; cf. CVE-2023-51385).
fn host_ok(s: &str) -> bool {
    if s.is_empty() || s.len() > 255 {
        return false;
    }
    if let Some(inner) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        let (addr, zone) = inner.split_once('%').unwrap_or((inner, "x"));
        return addr.contains(':')
            && addr
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || matches!(b, b':' | b'.'))
            && !zone.is_empty()
            && zone
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    }
    !s.starts_with('-')
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn path_ok(p: &str) -> Result<(), Error> {
    if p.is_empty() || p.contains('\n') || p.contains('\0') || p.contains('\r') {
        return errc(
            Code::Usage,
            "remote path is empty or contains a newline or NUL",
        );
    }
    Ok(())
}

/// A remote (ssh) path (Issue #45): absolute or `~/…` (so it can never be
/// read as an option of `serve`), printable ASCII (the §5.9.6 forced-command
/// grammar), and none of `\` or `!`, which some login shells (fish, csh)
/// treat specially even inside single quotes. It is always sent
/// single-quoted ([`shell_quote`]).
pub fn ssh_path_check(p: &str) -> Result<(), String> {
    if !(p.starts_with('/') || p == "~" || p.starts_with("~/")) {
        return Err(format!(
            "remote path {p:?} must be absolute or start with ~/"
        ));
    }
    if p.len() > 4096 {
        return Err("remote path is longer than 4096 bytes".into());
    }
    if let Some(c) = p
        .chars()
        .find(|c| !(' '..='~').contains(c) || matches!(c, '\\' | '!'))
    {
        return Err(format!(
            "remote path {p:?} contains {c:?} (control, non-ASCII, backslash and ! are refused)"
        ));
    }
    Ok(())
}

/// Parse a remote URL (§5.5). Local paths are resolved against `cwd`.
pub fn parse_url(s: &str, cwd: &Path) -> Result<Url, Error> {
    let usage = |m: &str| Error::new(Code::Usage, format!("bad remote url {s:?}: {m}"));
    if let Some(rest) = s.strip_prefix("file://") {
        if !rest.starts_with('/') {
            return Err(usage("file:// needs an absolute path"));
        }
        path_ok(rest)?;
        return Ok(Url::File(PathBuf::from(rest)));
    }
    let userhost = |a: &str| -> Result<(Option<String>, String), Error> {
        let (user, host) = match a.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, a),
        };
        if let Some(u) = user {
            if u.contains(':') {
                return Err(usage("passwords in URLs are refused; use ssh keys"));
            }
            if !user_ok(u) {
                return Err(usage("bad user (letters, digits, . _ - only)"));
            }
        }
        if !host_ok(host) {
            return Err(usage("bad host (a DNS name, IPv4, or [IPv6])"));
        }
        Ok((user.map(String::from), host.to_string()))
    };
    if let Some(rest) = s.strip_prefix("ssh://") {
        let (auth, path) = rest.split_at(rest.find('/').ok_or_else(|| usage("no path"))?);
        let (auth, port) = match auth.rsplit_once(':') {
            Some(_) if auth.ends_with(']') => (auth, None),
            Some((a, p)) => (a, Some(p.parse::<u16>().map_err(|_| usage("bad port"))?)),
            None => (auth, None),
        };
        let (user, host) = userhost(auth)?;
        let path = match path.strip_prefix("/~/") {
            Some(p) => format!("~/{p}"),
            None => path.to_string(),
        };
        ssh_path_check(&path).map_err(|m| usage(&m))?;
        return Ok(Url::Ssh {
            user,
            host,
            port,
            path,
        });
    }
    if s.contains("://") {
        if s.starts_with("https://") || s.starts_with("http://") {
            return errc(
                Code::Unsupported,
                format!("{s}: HTTP remotes are not implemented yet (serve --http is designed, not built)"),
            );
        }
        return Err(usage("unknown scheme"));
    }
    if let Some((a, p)) = s.split_once(':') {
        if !a.is_empty() && !a.contains('/') {
            let (user, host) = userhost(a)?;
            path_ok(p)?;
            let path = if p.starts_with('/') || p.starts_with('~') {
                p.to_string()
            } else {
                format!("~/{p}")
            };
            ssh_path_check(&path).map_err(|m| usage(&m))?;
            return Ok(Url::Ssh {
                user,
                host,
                port: None,
                path,
            });
        }
    }
    path_ok(s)?;
    Ok(Url::File(cwd.join(s)))
}

/// What `remote add` stores: local paths become `file://<canonical>`.
fn canonical_url(cwd: &Path, s: &str) -> Result<String, Error> {
    match parse_url(s, cwd)? {
        Url::File(p) => {
            if !p.is_dir() {
                return errc(
                    Code::NoSuchPath,
                    format!("no such directory: {}", p.display()),
                );
            }
            Ok(file_url(&fs::canonicalize(p)?))
        }
        Url::Ssh { .. } => Ok(s.to_string()),
    }
}

/// Single-quote for the remote shell (§5.6): `'` becomes `'\''`.
pub fn shell_quote(p: &str) -> String {
    format!("'{}'", p.replace('\'', "'\\''"))
}

/// The ssh argv (program first). `HUB_SSH_COMMAND` (tests) or
/// `BAROCSS_HUB_SSH` replace the program and fixed args (split on
/// whitespace, no shell).
pub fn ssh_argv(url: &Url, exec: Option<&str>, batch: bool) -> Result<Vec<String>, Error> {
    let Url::Ssh {
        user,
        host,
        port,
        path,
    } = url
    else {
        return errc(Code::Error, "internal: not an ssh url");
    };
    // Defense in depth (Issue #45): re-check every component that reaches
    // ssh or the remote shell, whatever produced `url` and `exec`.
    let unsafe_ = |m: String| Error::new(Code::Usage, format!("refusing to run ssh: {m}"));
    if user.as_deref().is_some_and(|u| !user_ok(u)) || !host_ok(host) {
        return Err(unsafe_("bad user or host".into()));
    }
    ssh_path_check(path).map_err(unsafe_)?;
    let exec = exec.unwrap_or("barocss-hub");
    exec_check(exec).map_err(unsafe_)?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let prog = std::env::var("HUB_SSH_COMMAND")
        .or_else(|_| std::env::var("BAROCSS_HUB_SSH"))
        .unwrap_or_else(|_| "ssh".into());
    let mut v: Vec<String> = prog.split_whitespace().map(String::from).collect();
    if v.is_empty() {
        v.push("ssh".into());
    }
    if let Some(p) = port {
        v.extend(["-p".into(), p.to_string()]);
    }
    if let Some(u) = user {
        v.extend(["-l".into(), u.clone()]);
    }
    v.extend([
        "-o".into(),
        format!("BatchMode={}", if batch { "yes" } else { "no" }),
        "--".into(),
        host.to_string(),
        format!("{exec} serve --stdio {}", shell_quote(path)),
    ]);
    Ok(v)
}

// ------------------------------------------------------------ sync

/// Resolve a `sync` argument (remote name, URL or path; none = default).
pub fn resolve(cwd: &Path, arg: Option<&str>) -> Result<Resolved, Error> {
    let hub = find_hub(cwd)?;
    let rs = remotes_view(&hub)?;
    let r = match arg {
        None => rs
            .iter()
            .find(|r| r.default)
            .or(rs.first())
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    Code::Usage,
                    "no remote configured; run `barocss-hub remote add <name> <url>` or give a url",
                )
            })?,
        Some(a) => match rs.iter().find(|r| r.name == a) {
            Some(r) => r.clone(),
            None => Remote {
                name: a.into(),
                url: a.into(),
                exec: None,
                default: false,
            },
        },
    };
    let root = hub.parent().unwrap_or(&hub).to_path_buf();
    let base = if arg.is_some() && r.url == r.name {
        cwd
    } else {
        &root
    };
    let url = parse_url(&r.url, base)?;
    // Configured = read from `.hub/remotes` (possibly planted), as opposed
    // to a URL or path typed on this command line.
    let configured = rs.iter().any(|x| x.name == r.name && x.url == r.url);
    Ok(Resolved {
        name: r.name,
        url,
        exec: r.exec,
        configured,
    })
}

pub struct Resolved {
    pub name: String,
    pub url: Url,
    pub exec: Option<String>,
    pub configured: bool,
}

/// How a first contact with an untrusted ssh remote is handled (#45).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustMode {
    /// `--yes`: record it and go on.
    Yes,
    /// Ask on the terminal (stdin and stderr are TTYs, no `--json`).
    Ask,
    /// Refuse with `untrusted-remote` (non-interactive, `watch`).
    Refuse,
}

/// The trust-store key of a remote as seen from the repository whose
/// canonical `.hub/` is `hub` (#45 repair: the local repository is part of
/// the key, so a planted copy of a trusted URL in another folder is new).
pub fn trust_key(r: &Remote, hub: &Path) -> Result<crate::trust::TrustKey, Error> {
    let root = hub.parent().unwrap_or(hub);
    Ok(key_of(&parse_url(&r.url, root)?, r.exec.as_deref(), hub))
}

/// `file://` targets are keyed by their canonical path (symlinks resolved;
/// a missing target keeps the literal path, and sync fails on it anyway).
/// `warnings --clear-reported [<remote>]` (Issue #50): forget the
/// remembered rejections of every remote, or of one remote (a configured
/// name, or a URL/path as `sync` takes it).
pub fn clear_reported(cwd: &Path, remote: Option<&str>) -> Result<usize, Error> {
    let hub = fs::canonicalize(find_hub(cwd)?)?;
    let key = match remote {
        None => None,
        Some(r) => {
            let res = resolve(cwd, Some(r))?;
            Some(match &res.url {
                Url::File(p) => match fs::canonicalize(p).ok().and_then(|p| find_hub(&p).ok()) {
                    Some(h) => file_key(&fs::canonicalize(h)?, &hub),
                    None => key_of(&res.url, None, &hub),
                },
                Url::Ssh { .. } => key_of(&res.url, res.exec.as_deref(), &hub),
            })
        }
    };
    crate::reported::forget(&hub, false, |e| key.as_ref().is_none_or(|k| e.key == *k))
}

/// The trust key of a `file://` remote whose canonical `.hub/` is
/// `theirs`, used from canonical `.hub/` `mine` (the #50 memory key).
pub(crate) fn file_key(theirs: &Path, mine: &Path) -> crate::trust::TrustKey {
    let root = theirs.parent().unwrap_or(theirs);
    key_of(&Url::File(root.to_path_buf()), None, mine)
}

/// The trust key of `url`/`exec` used from `.hub/` `hub` (#45).
pub(crate) fn key_of(url: &Url, exec: Option<&str>, hub: &Path) -> crate::trust::TrustKey {
    let local = fs::canonicalize(hub)
        .unwrap_or_else(|_| hub.to_path_buf())
        .to_string_lossy()
        .into_owned();
    match url {
        Url::File(p) => crate::trust::TrustKey {
            local,
            url: file_url(&fs::canonicalize(p).unwrap_or_else(|_| p.clone())),
            exec: "-".into(),
        },
        Url::Ssh { .. } => crate::trust::TrustKey {
            local,
            url: url_text(url),
            exec: exec.unwrap_or("barocss-hub").to_string(),
        },
    }
}

/// `remote add`/`set-url` are the user's own act: trust the result.
fn trust_added(r: &Remote, hub: &Path, out: &mut dyn Write) -> Result<(), Error> {
    let k = trust_key(r, hub)?;
    if let Err(e) = crate::trust::add(&k) {
        writeln!(
            out,
            "note: could not record {} as trusted ({e}); the first sync will ask",
            r.name
        )?;
    }
    Ok(())
}

/// The first-contact gate (Issue #45).
fn check_trust(res: &Resolved, mode: TrustMode, cwd: &Path) -> Result<(), Error> {
    if !res.configured {
        return Ok(());
    }
    let hub = fs::canonicalize(find_hub(cwd)?)?;
    let k = key_of(&res.url, res.exec.as_deref(), &hub);
    if crate::trust::is_trusted(&k).unwrap_or(false) {
        return Ok(());
    }
    let what = format!(
        "remote {} ({}) is configured in this repository but was never added or confirmed by you on this machine; .hub/remotes may come from a copied or downloaded folder",
        res.name,
        k.describe()
    );
    let go = match mode {
        TrustMode::Yes => true,
        TrustMode::Ask => {
            eprint!(
                "barocss-hub: {what}.
Trust it and connect now? [y/N] "
            );
            let mut l = String::new();
            let _ = std::io::stdin().read_line(&mut l);
            matches!(l.trim(), "y" | "Y" | "yes" | "YES" | "Yes")
        }
        TrustMode::Refuse => false,
    };
    if !go {
        return errc(
            Code::UntrustedRemote,
            format!(
                "{what}. Check it (`barocss-hub remote list`), then run `barocss-hub remote trust {}` or `barocss-hub sync --yes {}`",
                res.name, res.name
            ),
        );
    }
    // Recording can fail (no HOME): the confirmed sync still goes on.
    let _ = crate::trust::add(&k);
    Ok(())
}

/// The CLI's trust mode: `--yes`, else ask on a terminal, else refuse.
pub fn cli_trust_mode(yes: bool, json: bool) -> TrustMode {
    if yes {
        TrustMode::Yes
    } else if !json && stdin_is_tty() && stderr_is_tty() {
        TrustMode::Ask
    } else {
        TrustMode::Refuse
    }
}

/// Run one sync; `batch` = ssh BatchMode (non-interactive).
pub fn sync(
    cwd: &Path,
    arg: Option<&str>,
    batch: bool,
    trust: TrustMode,
    warn: &mut dyn Write,
) -> Result<Synced, Error> {
    let res = resolve(cwd, arg)?;
    check_trust(&res, trust, cwd)?;
    let Resolved { url, exec, .. } = res;
    match &url {
        Url::File(p) => sync::sync_local(cwd, p, sync::env_stop(), warn),
        Url::Ssh { .. } => sync_ssh(cwd, &url, exec.as_deref(), batch, warn),
    }
}

/// `barocss-hub sync [<remote|url>]` (human output).
pub fn sync_cmd(
    cwd: &Path,
    arg: Option<&str>,
    yes: bool,
    out: &mut dyn Write,
    warn: &mut dyn Write,
) -> Result<(), Error> {
    let r = sync(cwd, arg, !stdin_is_tty(), cli_trust_mode(yes, false), warn)?;
    sync::print_report(&r, out, warn)?;
    r.remember(warn)
}

fn is_tty(fd: i32) -> bool {
    #[cfg(unix)]
    {
        extern "C" {
            fn isatty(fd: std::os::raw::c_int) -> std::os::raw::c_int;
        }
        // SAFETY: isatty only inspects the descriptor.
        unsafe { isatty(fd) == 1 }
    }
    #[cfg(not(unix))]
    {
        let _ = fd;
        false
    }
}

fn stdin_is_tty() -> bool {
    is_tty(0)
}

fn stderr_is_tty() -> bool {
    is_tty(2)
}

fn url_text(url: &Url) -> String {
    match url {
        Url::File(p) => p.display().to_string(),
        Url::Ssh {
            user,
            host,
            port,
            path,
        } => format!(
            "ssh://{}{host}{}{}{path}",
            user.as_ref().map(|u| format!("{u}@")).unwrap_or_default(),
            port.map(|p| format!(":{p}")).unwrap_or_default(),
            if path.starts_with('/') { "" } else { "/" }
        ),
    }
}

fn sync_ssh(
    cwd: &Path,
    url: &Url,
    exec: Option<&str>,
    batch: bool,
    warn: &mut dyn Write,
) -> Result<Synced, Error> {
    let repo = Repo::discover(cwd)?;
    for w in &repo.warnings {
        writeln!(warn, "warning: {w}")?;
    }
    let mut side = Side::new(Access::Held(Box::new(repo)), false)?;
    side.stop = sync::env_stop();
    let argv = ssh_argv(url, exec, batch)?;
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::new(Code::Io, format!("cannot run {}: {e}", argv[0])))?;
    let tail = Arc::new(Mutex::new(Vec::<u8>::new()));
    let t2 = tail.clone();
    let mut err = child.stderr.take().expect("piped");
    let errt = std::thread::spawn(move || {
        let mut b = [0u8; 4096];
        while let Ok(n) = err.read(&mut b) {
            if n == 0 {
                break;
            }
            let mut t = t2.lock().unwrap();
            t.extend_from_slice(&b[..n]);
            let l = t.len();
            if l > 4096 {
                t.drain(..l - 4096);
            }
        }
    });
    let mut conn = Conn::from_io(
        child.stdout.take().expect("piped"),
        child.stdin.take().expect("piped"),
    );
    let r = session::client(&mut conn, &mut side);
    drop(conn);
    if r.is_err() {
        let _ = child.kill();
    }
    let st = child.wait()?;
    let _ = errt.join();
    let tail = String::from_utf8_lossy(&tail.lock().unwrap())
        .trim()
        .to_string();
    let report = match r {
        Ok(rep) => rep,
        Err(e) if e.is_remote() => return Err(e),
        Err(e) => {
            let hint = match st.code() {
                Some(127) => " (remote barocss-hub not found; set the remote's exec path: `remote add --exec /abs/path/barocss-hub ...`)",
                Some(c) if c != 0 && e.code() == Code::Protocol => " (remote barocss-hub has no serve (too old?) or printed a banner)",
                _ => "",
            };
            let code = if matches!(e.code(), Code::Io) || st.code() == Some(127) {
                Code::Io
            } else {
                e.code()
            };
            let mut m = format!("sync with {} failed: {}{hint}", url_text(url), e.0);
            if !tail.is_empty() {
                m.push_str(&format!("; ssh: {tail}"));
            }
            return Err(Error::new(code, m));
        }
    };
    let hub = fs::canonicalize(find_hub(cwd)?)?;
    let key = key_of(url, exec, &hub);
    sync::summarize(url_text(url), &side, report, None, Some(key))
}

// ------------------------------------------------------------ serve

/// Parse `SSH_ORIGINAL_COMMAND` with the exact §5.9.6 grammar. Returns
/// (`--read-only` given, path).
pub fn parse_forced(cmd: &str) -> Result<(bool, String), Error> {
    let no = || {
        Error::new(Code::Forbidden, "forced command: SSH_ORIGINAL_COMMAND is not `<bin> serve --stdio [--read-only] '<path>'`")
    };
    if !cmd.is_ascii() {
        return Err(no());
    }
    let (word, rest) = cmd.split_once(' ').ok_or_else(no)?;
    if word.is_empty()
        || !word
            .bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && b != b'\'')
    {
        return Err(no());
    }
    let rest = rest.strip_prefix("serve --stdio ").ok_or_else(no)?;
    let (ro, q) = match rest.strip_prefix("--read-only ") {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let b = q.as_bytes();
    if b.len() < 3 || b[0] != b'\'' || b[b.len() - 1] != b'\'' {
        return Err(no());
    }
    let inner = &b[1..b.len() - 1];
    let mut out = Vec::new();
    let mut i = 0;
    while i < inner.len() {
        if inner[i..].starts_with(b"'\\''") {
            out.push(b'\'');
            i += 4;
        } else if (0x20..=0x7e).contains(&inner[i]) && inner[i] != b'\'' {
            out.push(inner[i]);
            i += 1;
        } else {
            return Err(no());
        }
    }
    if out.is_empty() {
        return Err(no());
    }
    Ok((ro, String::from_utf8(out).map_err(|_| no())?))
}

fn forbidden(m: impl Into<String>) -> Error {
    Error::new(Code::Forbidden, m)
}

/// Resolve and confine a served path (§5.9.2). Returns the canonical
/// `.hub/` directory. Nothing is opened for writing here.
pub fn confine(
    path: &str,
    root: Option<&Path>,
    forced: bool,
    cwd: &Path,
) -> Result<PathBuf, Error> {
    if path.is_empty() || path.contains('\0') || path.contains('\n') {
        return Err(forbidden("bad path"));
    }
    let p: PathBuf = if path == "~" || path.starts_with("~/") {
        let home = std::env::var_os("HOME").ok_or_else(|| forbidden("no home directory"))?;
        PathBuf::from(home).join(path.trim_start_matches('~').trim_start_matches('/'))
    } else if path.starts_with('~') {
        return Err(forbidden(format!("{path}: ~user paths are refused")));
    } else {
        PathBuf::from(path)
    };
    if forced && !p.is_absolute() {
        return Err(forbidden(format!("{path}: must be absolute or ~/")));
    }
    if Path::new(path)
        .components()
        .any(|c| c == Component::ParentDir)
    {
        return Err(forbidden(format!("{path}: `..` is refused")));
    }
    let p = if p.is_absolute() { p } else { cwd.join(p) };
    let croot = match root {
        Some(r) => Some(
            fs::canonicalize(r)
                .map_err(|e| forbidden(format!("serve root {}: {e}", r.display())))?,
        ),
        None => None,
    };
    let canon = match fs::canonicalize(&p) {
        Ok(c) => c,
        Err(_) => return errc(Code::NotARepository, format!("no repository at {path}")),
    };
    if let Some(r) = &croot {
        if !canon.starts_with(r) {
            return Err(forbidden(format!("{path} is outside the serve root")));
        }
    }
    let hub = canon.join(crate::HUB_DIR);
    let meta = fs::symlink_metadata(&hub)
        .map_err(|_| Error::new(Code::NotARepository, format!("no repository at {path}")))?;
    let inside = |q: &Path| -> Result<(), Error> {
        let t = fs::canonicalize(q)
            .map_err(|_| forbidden(format!("{}: dangling symlink", q.display())))?;
        if !t.starts_with(&canon) {
            return Err(forbidden(format!(
                "{} is a symlink pointing outside the repository",
                q.display()
            )));
        }
        Ok(())
    };
    if meta.file_type().is_symlink() {
        inside(&hub)?;
    }
    if !hub.is_dir() {
        return errc(Code::NotARepository, format!("no repository at {path}"));
    }
    let chub = fs::canonicalize(&hub)?;
    for d in [chub.clone(), chub.join("blobs")] {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
                inside(&e.path())?;
            }
        }
    }
    if !chub.join("replica").is_file() {
        return errc(Code::NotARepository, format!("no repository at {path}"));
    }
    Ok(chub)
}

/// `barocss-hub serve --stdio [--read-only] [--root <dir>]
/// (--from-ssh-original-command | <path>)`. Speaks the protocol on
/// stdin/stdout; diagnostics go to stderr.
pub fn serve_cmd(cwd: &Path, args: &[&str]) -> Result<(), Error> {
    let usage = || {
        Error::new(
            Code::Usage,
            "usage: barocss-hub serve --stdio [--read-only] [--root <dir>] (<path> | --from-ssh-original-command)",
        )
    };
    let (mut stdio, mut ro, mut forced) = (false, false, false);
    let mut root: Option<PathBuf> = None;
    let mut path: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match *a {
            "--stdio" => stdio = true,
            "--read-only" => ro = true,
            "--from-ssh-original-command" => forced = true,
            "--root" => root = Some(PathBuf::from(it.next().ok_or_else(usage)?)),
            p if !p.starts_with("--") && path.is_none() => path = Some(p.to_string()),
            _ => return Err(usage()),
        }
    }
    if !stdio {
        return Err(usage());
    }
    let mut conn = Conn::from_io(std::io::stdin(), std::io::stdout());
    let mut prep = || -> Result<PathBuf, Error> {
        let p = if forced {
            if path.is_some() {
                return Err(forbidden(
                    "a path argument is not allowed with --from-ssh-original-command",
                ));
            }
            let c = std::env::var("SSH_ORIGINAL_COMMAND")
                .map_err(|_| forbidden("forced command without SSH_ORIGINAL_COMMAND"))?;
            let (r, p) = parse_forced(&c)?;
            ro |= r;
            p
        } else {
            if root.is_none() {
                root = std::env::var_os("BAROCSS_HUB_SERVE_ROOT").map(PathBuf::from);
            }
            path.clone().ok_or_else(usage)?
        };
        confine(&p, root.as_deref(), forced, cwd)
    };
    let dir = match prep() {
        Ok(d) => d,
        Err(e) => {
            // Before Hello: the preamble and one fatal Error, so the client
            // reports the code instead of a bare EOF.
            let _ = conn.send_magic();
            conn.send_error(&e, true);
            return Err(e);
        }
    };
    let mut side = match Side::new(Access::Path(dir), ro) {
        Ok(s) => s,
        Err(e) => {
            let _ = conn.send_magic();
            conn.send_error(&e, true);
            return Err(e);
        }
    };
    let wall = hook_var!("HUB_TEST_SESSION_MS")
        .and_then(|v| v.parse().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(std::time::Duration::from_secs(3600));
    conn.deadline = Some(std::time::Instant::now() + wall);
    session::server(&mut conn, &mut side)
}

/// True for a bare repository (`.hub/bare`, D-042).
pub fn is_bare(hub: &Path) -> bool {
    hub.join(BARE_FILE).exists()
}

/// Refuse a working-directory command in a bare repository.
pub fn refuse_bare(hub: &Path, cmd: &str) -> Result<(), Error> {
    if is_bare(hub) {
        return errc(
            Code::Bare,
            format!("`{cmd}` needs a working directory; this is a bare repository"),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_grammar() {
        let ok = |c: &str| parse_forced(c).unwrap();
        assert_eq!(
            ok("barocss-hub serve --stdio '/srv/a b'"),
            (false, "/srv/a b".into())
        );
        assert_eq!(
            ok("x serve --stdio --read-only '~/r'"),
            (true, "~/r".into())
        );
        assert_eq!(ok("x serve --stdio 'it'\\''s'"), (false, "it's".into()));
        for bad in [
            "x serve --stdio /a",
            "x  serve --stdio '/a'",
            "x serve --stdio '/a' ",
            "x serve --stdio \"/a\"",
            "x serve --stdio --root / '/a'",
            "x serve --stdio '/a'; rm -rf /",
            "x serve --stdio '/a' '/b'",
            "x serve --stdio ''",
            "x serve --stdio '/\u{e9}'",
            "x serve --stdio '/a\tb'",
            "'x' serve --stdio '/a'",
            "x serve --stdio '/a\\b'x",
            "serve --stdio '/a'",
        ] {
            assert_eq!(
                parse_forced(bad).unwrap_err().code(),
                Code::Forbidden,
                "{bad}"
            );
        }
        // Backslash inside a quoted path is a plain character.
        assert_eq!(ok("x serve --stdio '/a\\b'"), (false, "/a\\b".into()));
    }

    #[test]
    fn urls() {
        let c = Path::new("/w");
        assert_eq!(
            parse_url("ssh://al@h:2222/srv/p", c).unwrap(),
            Url::Ssh {
                user: Some("al".into()),
                host: "h".into(),
                port: Some(2222),
                path: "/srv/p".into()
            }
        );
        assert_eq!(
            parse_url("ssh://h/~/p", c).unwrap(),
            Url::Ssh {
                user: None,
                host: "h".into(),
                port: None,
                path: "~/p".into()
            }
        );
        assert_eq!(
            parse_url("h:rel/p", c).unwrap(),
            Url::Ssh {
                user: None,
                host: "h".into(),
                port: None,
                path: "~/rel/p".into()
            }
        );
        assert_eq!(parse_url("file:///x", c).unwrap(), Url::File("/x".into()));
        assert_eq!(parse_url("../b", c).unwrap(), Url::File("/w/../b".into()));
        assert_eq!(
            parse_url("https://h/p", c).unwrap_err().code(),
            Code::Unsupported
        );
        for bad in [
            "ssh://u:pw@h/p",
            "ssh://-oProxyCommand=x/p",
            "-oProxyCommand=x:p",
            "ftp://h/p",
            "ssh://h/a\nb",
            "file://rel",
        ] {
            assert!(parse_url(bad, c).is_err(), "{bad}");
        }
        // Issue #45: bracketed IPv6 (with and without a port), strict
        // user/host, and the path rules.
        let v6 = parse_url("ssh://[fe80::1%en0]:2222/srv", c).unwrap();
        assert_eq!(
            v6,
            Url::Ssh {
                user: None,
                host: "[fe80::1%en0]".into(),
                port: Some(2222),
                path: "/srv".into()
            }
        );
        let a = ssh_argv(&parse_url("ssh://[::1]/srv", c).unwrap(), None, true).unwrap();
        assert_eq!(a[a.len() - 2], "::1");
        assert!(!a.contains(&"-p".to_string()));
        for bad in [
            "ssh://h%x/p",
            "ssh://[::1;x]/p",
            "ssh://[h]/p",
            "ssh://u%x@h/p",
            "ssh://.h/p",
            "ssh://h/p!x",
            "ssh://h/p\\x",
            "ssh://h/p\tx",
            "ssh://h/\u{e9}",
            "h:~root/p",
        ] {
            assert!(parse_url(bad, c).is_err(), "{bad}");
        }
        assert!(exec_check("barocss-hub").is_ok());
        assert!(exec_check("~/.cargo/bin/barocss-hub").is_ok());
        for bad in [
            "", "-x", "a b", "a;b", "a$b", "a`b", "a'b", "a\"b", "a|b", "a\nb", "a\\b", "é",
        ] {
            assert!(exec_check(bad).is_err(), "{bad:?}");
        }
        // ssh_argv re-checks whatever it is given.
        let u = Url::Ssh {
            user: None,
            host: "h".into(),
            port: None,
            path: "/p".into(),
        };
        assert!(ssh_argv(&u, Some("x;id"), true).is_err());
        let evil = Url::Ssh {
            user: Some("-oProxyCommand=id".into()),
            host: "h".into(),
            port: None,
            path: "/p".into(),
        };
        assert!(ssh_argv(&evil, None, true).is_err());
        let a = ssh_argv(&parse_url("h:/it's; rm -rf ~", c).unwrap(), None, true).unwrap();
        let last = a.last().unwrap();
        assert_eq!(last, "barocss-hub serve --stdio '/it'\\''s; rm -rf ~'");
        assert_eq!(parse_forced(last).unwrap().1, "/it's; rm -rf ~");
    }
}
