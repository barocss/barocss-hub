//! Trusted remotes (Issue #45, [decision-proposal]).
//!
//! `.hub/remotes` travels with a folder: a repository someone hands you may
//! name an attacker's host or `exec`. So the first contact with an ssh
//! remote that this user never added or confirmed on this machine is not
//! automatic: the CLI asks (on a terminal), needs `--yes`, or refuses with
//! `untrusted-remote`; `watch` skips it with an event.
//!
//! The store is per user, outside every repository:
//! `$XDG_CONFIG_HOME/barocss-hub/trusted-remotes` (when `XDG_CONFIG_HOME`
//! is an absolute path), else `$HOME/.config/barocss-hub/trusted-remotes`
//! (on macOS too, for one documented location). Header
//! `barocss-hub/trusted-remotes/v0`, then `T <url> <exec>` per entry
//! (escaped like `.hub/remotes`). The key is the normalized URL
//! (`ssh://[user@]host[:port]/path`) plus the exec word, so a changed host,
//! user, port, path or exec is a new remote. `remote add`, `remote
//! set-url`, `remote trust` and a confirmed or `--yes` first contact add
//! entries. `file://` remotes are local paths and are not gated.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use crate::{store, Code, Error};

pub const HEADER: &str = "barocss-hub/trusted-remotes/v0";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustKey {
    pub url: String,
    pub exec: String,
}

impl TrustKey {
    pub fn describe(&self) -> String {
        format!("{}, exec {}", self.url, self.exec)
    }
}

/// The store file, or an error when neither variable names a place.
pub fn store_path() -> Result<PathBuf, Error> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        Some(x) if x.is_absolute() => x,
        _ => match std::env::var_os("HOME").map(PathBuf::from) {
            Some(h) if h.is_absolute() => h.join(".config"),
            _ => {
                return Err(Error::new(
                    Code::UntrustedRemote,
                    "cannot locate the trusted-remotes store: neither XDG_CONFIG_HOME nor HOME is an absolute path",
                ))
            }
        },
    };
    Ok(base.join("barocss-hub").join("trusted-remotes"))
}

fn read_all() -> Result<Vec<TrustKey>, Error> {
    let p = store_path()?;
    let text = match fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let bad = || Error::new(Code::Corrupt, format!("corrupt {}", p.display()));
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        return Err(bad());
    }
    lines
        .filter(|l| !l.is_empty())
        .map(|l| {
            let t: Vec<&str> = l.split(' ').collect();
            let ["T", u, e] = t.as_slice() else {
                return Err(bad());
            };
            Ok(TrustKey {
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
    let mut all = read_all()?;
    if all.contains(k) {
        return Ok(());
    }
    all.push(k.clone());
    let p = store_path()?;
    let dir = p.parent().expect("store path has a parent");
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    let mut s = format!("{HEADER}\n");
    for t in &all {
        s.push_str(&format!(
            "T {} {}\n",
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
    Ok(())
}
