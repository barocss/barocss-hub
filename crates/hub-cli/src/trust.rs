//! Trusted remotes (Issue #45, [decision-proposal]).
//!
//! `.hub/remotes` travels with a folder: a repository someone hands you may
//! name an attacker's host or `exec`, or (`file://`) another of your own
//! repositories. So the first contact with any remote read from
//! `.hub/remotes` that this user never added or confirmed *from this
//! repository* is not automatic: the CLI asks (on a terminal), needs
//! `--yes`, or refuses with `untrusted-remote`; `watch` skips it with an
//! event.
//!
//! The store is per user, outside every repository (D-046):
//! `~/Library/Application Support/barocss-hub/trusted-remotes` on macOS;
//! elsewhere `$XDG_CONFIG_HOME/barocss-hub/trusted-remotes` (when
//! `XDG_CONFIG_HOME` is an absolute path), else
//! `$HOME/.config/barocss-hub/trusted-remotes`. `BAROCSS_HUB_CONFIG_DIR`
//! (absolute) overrides the directory everywhere. On macOS an existing
//! pre-D-046 `~/.config/barocss-hub/trusted-remotes` is read too and moved
//! into the new store on the next write.
//! Header `barocss-hub/trusted-remotes/v1`, then `T <local> <url> <exec>`
//! per entry (escaped like `.hub/remotes`). The key is the canonical local
//! `.hub/`, the normalized URL (`ssh://[user@]host[:port]/path`, or
//! `file://<canonical target>`) and the exec word (`-` for `file://`).
//! A v0 file (first #45 round, no local path) matches nothing. `remote
//! add`, `remote set-url`, `remote trust` and a confirmed or `--yes` first
//! contact add entries; the peers -> remotes migration adds none.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{store, Code, Error};

pub const HEADER: &str = "barocss-hub/trusted-remotes/v1";
/// The #45 first-round format (no local repository in the key). Its
/// entries no longer match anything: each remote needs one confirmation
/// again, and the next write replaces the file with v1.
pub const HEADER_V0: &str = "barocss-hub/trusted-remotes/v0";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustKey {
    /// Canonical `.hub/` of the repository the remote is used from.
    pub local: String,
    pub url: String,
    pub exec: String,
}

impl TrustKey {
    pub fn describe(&self) -> String {
        if self.exec == "-" {
            format!("{}, from {}", self.url, self.local)
        } else {
            format!("{}, exec {}, from {}", self.url, self.exec, self.local)
        }
    }
}

/// Environment override for the store directory (D-046): an absolute path
/// to the directory holding `trusted-remotes`. Meant for tests and unusual
/// setups; a relative value is ignored.
pub const ENV_DIR: &str = "BAROCSS_HUB_CONFIG_DIR";

fn abs_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// The XDG location: `$XDG_CONFIG_HOME/barocss-hub` (when absolute), else
/// `$HOME/.config/barocss-hub`. The store on Linux and other Unixes, and
/// the pre-D-046 location on macOS (read for migration).
fn xdg_dir() -> Option<PathBuf> {
    abs_env("XDG_CONFIG_HOME")
        .or_else(|| abs_env("HOME").map(|h| h.join(".config")))
        .map(|b| b.join("barocss-hub"))
}

/// The store directory (D-046): [`ENV_DIR`] if set; on macOS
/// `$HOME/Library/Application Support/barocss-hub` (`XDG_CONFIG_HOME` is
/// not consulted there, as for other macOS applications); elsewhere
/// [`xdg_dir`].
fn store_dir() -> Option<PathBuf> {
    if let Some(d) = abs_env(ENV_DIR) {
        return Some(d);
    }
    if cfg!(target_os = "macos") {
        abs_env("HOME").map(|h| h.join("Library/Application Support/barocss-hub"))
    } else {
        xdg_dir()
    }
}

/// The pre-D-046 store on macOS (`~/.config/barocss-hub/trusted-remotes`,
/// or under `XDG_CONFIG_HOME`), when it differs from the current one. Its
/// entries are read as well; the next write merges them into the current
/// store and removes the old file (a move). Not used with [`ENV_DIR`].
fn legacy_path() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") || abs_env(ENV_DIR).is_some() {
        return None;
    }
    let p = xdg_dir()?.join("trusted-remotes");
    (Some(p.clone()) != store_dir().map(|d| d.join("trusted-remotes"))).then_some(p)
}

/// The store file, or an error when no variable names a place.
pub fn store_path() -> Result<PathBuf, Error> {
    match store_dir() {
        Some(d) => Ok(d.join("trusted-remotes")),
        None => Err(Error::new(
            Code::UntrustedRemote,
            format!(
                "cannot locate the trusted-remotes store: HOME is not an absolute path (nor {ENV_DIR}{})",
                if cfg!(target_os = "macos") { "" } else { ", XDG_CONFIG_HOME" }
            ),
        )),
    }
}

fn read_all() -> Result<Vec<TrustKey>, Error> {
    let mut all = read_file(&store_path()?)?;
    if let Some(l) = legacy_path() {
        for k in read_file(&l)? {
            if !all.contains(&k) {
                all.push(k);
            }
        }
    }
    Ok(all)
}

fn read_file(p: &Path) -> Result<Vec<TrustKey>, Error> {
    let text = match fs::read_to_string(p) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let bad = || Error::new(Code::Corrupt, format!("corrupt {}", p.display()));
    let mut lines = text.lines();
    match lines.next() {
        Some(HEADER) => {}
        Some(HEADER_V0) => return Ok(Vec::new()),
        _ => return Err(bad()),
    }
    lines
        .filter(|l| !l.is_empty())
        .map(|l| {
            let t: Vec<&str> = l.split(' ').collect();
            let ["T", l, u, e] = t.as_slice() else {
                return Err(bad());
            };
            Ok(TrustKey {
                local: store::unesc(l).map_err(|_| bad())?,
                url: store::unesc(u).map_err(|_| bad())?,
                exec: store::unesc(e).map_err(|_| bad())?,
            })
        })
        .collect()
}

pub fn is_trusted(k: &TrustKey) -> Result<bool, Error> {
    Ok(read_all()?.contains(k))
}

/// Add `k` (temp + fsync + rename; the directory is created 0700).
pub fn add(k: &TrustKey) -> Result<(), Error> {
    // Read-modify-write under an exclusive lock on the store directory, so
    // concurrent `remote add`/`trust` from several repos lose no entry.
    let p = store_path()?;
    let dir = p.parent().expect("store path has a parent");
    fs::create_dir_all(dir)?;
    let _lock = store::RepoLock::acquire(dir, store::LockMode::Exclusive)?;
    let mut all = read_all()?;
    let legacy = legacy_path().filter(|l| l.exists());
    if all.contains(k) && legacy.is_none() {
        return Ok(());
    }
    if !all.contains(k) {
        all.push(k.clone());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    let mut s = format!("{HEADER}\n");
    for t in &all {
        s.push_str(&format!(
            "T {} {} {}\n",
            store::esc(&t.local),
            store::esc(&t.url),
            store::esc(&t.exec)
        ));
    }
    let tmp = dir.join(format!(".trusted-remotes.{}.tmp", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(s.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &p)?;
    if let Some(l) = legacy {
        // Issue #46: make the rename durable (directory entry flushed, then
        // a barrier) before the old macOS file goes, so a crash in between
        // can never leave neither file holding the merged entries.
        let d = fs::File::open(dir)?;
        crate::durable::flush_dir(dir, &d)?;
        crate::durable::barrier(&d)?;
        crate::durable::crash_point(crate::durable::TRUST_MOVED);
        // Merged above: the old macOS file has moved.
        let _ = fs::remove_file(l);
    }
    Ok(())
}
