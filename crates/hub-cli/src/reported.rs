//! Memory of already-reported rejected ops (Issue #50, D-050).
//!
//! `.hub/reported-rejections` (local, never replicated): header
//! [`HEADER`], then one framed record per rejection already shown to the
//! user: `R <local> <url> <exec> <op> <payload-sha256> <reason>`, where
//! `(local, url, exec)` is the remote's #45 trust key and the payload hash
//! is SHA-256 of this repository's encoded op ([`crate::store::encode_op`]).
//! A later sync's rejection matching a record on all of key, op, payload
//! hash and reason is "already reported": no warning, no exit 3 (D-045);
//! anything else is new. `duplicate-replica` is never remembered, and the
//! memory only dedups the rejected-op *report*: equivocation and
//! DuplicateReplica warnings (D-008/D-022) are unaffected.
//!
//! Durability: the memory is written only after the report was printed
//! (a crash before that leaves it unwritten: the next sync reports again,
//! never a false suppression), under the exclusive repository lock, as
//! temp + flush + barrier + rename + directory sync, with crash points
//! [`REPORTED_TMP`] and [`REPORTED_RENAMED`]. A torn or corrupt file is
//! ignored (reports again) rather than failing the sync.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use hub_core::tree::OpId;
use sha2::{Digest, Sha256};

use crate::store::{self, LockMode};
use crate::trust::TrustKey;
use crate::{durable, Error, Repo};

pub const FILE: &str = "reported-rejections";
pub const HEADER: &str = "barocss-hub/reported-rejections/v0";
/// Temp written, flushed and barriered; not renamed.
pub const REPORTED_TMP: &str = "reported-tmp";
/// Renamed into place, `.hub/` not yet synced.
pub const REPORTED_RENAMED: &str = "reported-renamed";

/// One rejection of a sync, classified against the memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejection {
    pub op: OpId,
    pub reason: String,
    /// SHA-256 hex of our encoded op (None: we do not hold it; never
    /// remembered, always reported).
    pub payload: Option<String>,
    pub already: bool,
}

/// A remembered rejection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub key: TrustKey,
    pub op: OpId,
    pub payload: String,
    pub reason: String,
}

fn encode(e: &Entry) -> String {
    format!(
        "R {} {} {} {} {} {}",
        store::esc(&e.key.local),
        store::esc(&e.key.url),
        store::esc(&e.key.exec),
        store::fmt_id(e.op),
        e.payload,
        store::esc(&e.reason)
    )
}

fn decode(b: &str) -> Result<Entry, String> {
    let t: Vec<&str> = b.split(' ').collect();
    let ["R", local, url, exec, op, payload, reason] = t.as_slice() else {
        return Err("not a reported-rejection record".into());
    };
    if payload.len() != 64 || !payload.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("bad payload hash".into());
    }
    Ok(Entry {
        key: TrustKey {
            local: store::unesc(local)?,
            url: store::unesc(url)?,
            exec: store::unesc(exec)?,
        },
        op: store::parse_id(op)?,
        payload: payload.to_string(),
        reason: store::unesc(reason)?,
    })
}

/// The remembered rejections of `.hub/` `dir`. Missing, torn or corrupt
/// records are skipped (they only cause a repeat report).
pub fn read(dir: &Path) -> Vec<Entry> {
    let p = dir.join(FILE);
    if !p.exists() {
        return Vec::new();
    }
    match store::read_log(&p, HEADER) {
        Ok(l) => l.records.iter().filter_map(|b| decode(b).ok()).collect(),
        Err(_) => Vec::new(),
    }
}

fn write(dir: &Path, es: &[Entry]) -> Result<(), Error> {
    let mut s = format!("{HEADER}\n");
    for e in es {
        s.push_str(&store::frame(&encode(e)));
    }
    let tmp = dir.join(format!(".{FILE}.tmp"));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(s.as_bytes())?;
        durable::flush(&f)?;
        durable::barrier(&f)?;
    }
    durable::crash_point(REPORTED_TMP);
    fs::rename(&tmp, dir.join(FILE))?;
    durable::crash_point(REPORTED_RENAMED);
    let d = File::open(dir)?;
    durable::flush_dir(dir, &d)?;
    durable::barrier(&d)?;
    Ok(())
}

/// SHA-256 hex of an op's canonical encoding.
pub fn payload_hash(op: &hub_core::tree::Op) -> String {
    let d: [u8; 32] = Sha256::digest(store::encode_op(op).as_bytes()).into();
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether a reason may be suppressed at all.
fn suppressible(reason: &str) -> bool {
    reason != "duplicate-replica"
}

/// Classify a sync's rejections against the memory in `dir` for `key`.
pub fn classify(repo: &Repo, key: Option<&TrustKey>, rej: &[(OpId, String)]) -> Vec<Rejection> {
    let mem = key.map(|_| read(&repo.dir)).unwrap_or_default();
    rej.iter()
        .map(|(op, reason)| {
            let payload = repo.store.get(op).map(payload_hash);
            let already = DEDUP.with(|d| d.get())
                && suppressible(reason)
                && match (key, &payload) {
                    (Some(k), Some(h)) => mem.iter().any(|e| {
                        e.key == *k && e.op == *op && e.payload == *h && e.reason == *reason
                    }),
                    _ => false,
                };
            Rejection {
                op: *op,
                reason: reason.clone(),
                payload,
                already,
            }
        })
        .collect()
}

thread_local! {
    /// Test-only switch (mutation check): false disables suppression.
    /// Honored only in debug/test builds (#50 follow-up): a release build
    /// ignores `HUB_TEST_NO_REJECTION_DEDUP`.
    static DEDUP: std::cell::Cell<bool> = std::cell::Cell::new(
        !cfg!(debug_assertions) || std::env::var_os("HUB_TEST_NO_REJECTION_DEDUP").is_none()
    );
}

/// Remove a stale `.reported-rejections.tmp` (crash between temp and
/// rename). Caller holds the exclusive lock (#50 follow-up).
pub fn sweep_tmp(dir: &Path) {
    let _ = fs::remove_file(dir.join(format!(".{FILE}.tmp")));
}

/// Remember the new rejections of a finished, already printed sync.
/// Takes the exclusive lock of `.hub/` `dir`.
pub fn remember(dir: &Path, key: Option<&TrustKey>, rej: &[Rejection]) -> Result<(), Error> {
    let Some(key) = key else { return Ok(()) };
    let add: Vec<Entry> = rej
        .iter()
        .filter(|r| !r.already && suppressible(&r.reason))
        .filter_map(|r| {
            Some(Entry {
                key: key.clone(),
                op: r.op,
                payload: r.payload.clone()?,
                reason: r.reason.clone(),
            })
        })
        .collect();
    if add.is_empty() {
        return Ok(());
    }
    let _lock = store::RepoLock::acquire(dir, LockMode::Exclusive)?;
    let mut es = read(dir);
    let n = es.len();
    for e in add {
        if !es.contains(&e) {
            es.push(e);
        }
    }
    if es.len() == n {
        return Ok(());
    }
    write(dir, &es)
}

/// Forget remembered rejections matching `keep_not` (the clear option and
/// `reid --rescue`, #49). Returns how many were removed. Takes the
/// exclusive lock unless the caller already holds it (`locked`).
pub fn forget(dir: &Path, locked: bool, drop: impl Fn(&Entry) -> bool) -> Result<usize, Error> {
    let _lock = if locked {
        None
    } else {
        Some(store::RepoLock::acquire(dir, LockMode::Exclusive)?)
    };
    let es = read(dir);
    let keep: Vec<Entry> = es.iter().filter(|e| !drop(e)).cloned().collect();
    let n = es.len() - keep.len();
    if n > 0 {
        write(dir, &keep)?;
    }
    Ok(n)
}
