//! On-disk format migration v0 -> v1 (Issue #33, Wiki Appendix A.2.6).
//!
//! Runs under the exclusive repo lock. Before the switch (the atomic
//! rename of a v1 `replica` file) every v0 file is untouched, so a crash
//! leaves a working v0 repository and the next v1 open starts over. After
//! the switch, [`finish`] rolls the remaining steps forward on every open
//! until they are done.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

use hub_core::tree::Replica;

use crate::store::{self, LogPos};
use crate::{durable, err, watch, Error};

pub const OPLOG_V1_TMP: &str = "ops.log.v1";

/// What the `replica` file says.
#[derive(Debug, PartialEq, Eq)]
pub enum Format {
    V0,
    V1,
}

/// Parse `.hub/replica`: format and replica id.
pub fn read_replica(dir: &Path) -> Result<(Format, u128), Error> {
    let rp = dir.join("replica");
    let rtext =
        fs::read_to_string(&rp).map_err(|e| Error(format!("cannot read {}: {e}", rp.display())))?;
    let lines: Vec<&str> = rtext.lines().collect();
    let fmt = match lines.first() {
        Some(h) if *h == store::REPLICA_HEADER => Format::V1,
        Some(h) if *h == store::REPLICA_HEADER_V0 => Format::V0,
        Some(h) if h.starts_with(store::REPLICA_HEADER_PREFIX) => {
            return err(format!(
                "unsupported repository format '{}' in {} (this hub understands v0 and v1; use a newer hub)",
                &h[store::REPLICA_HEADER_PREFIX.len()..],
                rp.display()
            ))
        }
        _ => return err(format!("corrupt {}", rp.display())),
    };
    match lines.as_slice() {
        [_, id] if id.len() == 32 => Ok((
            fmt,
            u128::from_str_radix(id, 16).map_err(|_| Error("bad replica id".into()))?,
        )),
        _ => err(format!("corrupt {}", rp.display())),
    }
}

/// Roll-forward work left after the switch (a crash mid-migration).
pub fn needs_finish(dir: &Path) -> bool {
    dir.join(OPLOG_V1_TMP).exists()
        || !dir.join(watch::LOCK_FILE).exists()
        || !dir.join("format").exists()
        || fs::read_to_string(dir.join("peers"))
            .map(|t| t.lines().next() != Some(watch::PEERS_HEADER))
            .unwrap_or(true)
}

/// Check `.hub/format` if present (must say v1).
pub fn check_format(dir: &Path) -> Result<(), Error> {
    match fs::read_to_string(dir.join("format")) {
        Ok(t) if t == store::FORMAT_V1 => Ok(()),
        Ok(t) => err(format!(
            "unsupported repository format '{}' in {} (this hub understands v0 and v1; use a newer hub)",
            t.trim(),
            dir.join("format").display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn dsync(dir: &Path) -> Result<(), Error> {
    let d = File::open(dir)?;
    durable::flush_dir(dir, &d)?;
    durable::barrier(&d)?;
    Ok(())
}

/// Steps 1-4 of the migration. Returns the replayed replica and log
/// position (for the step-5 snapshot). Caller holds the exclusive lock and
/// has seen a v0 `replica`.
pub fn migrate(dir: &Path) -> Result<(Replica, LogPos), Error> {
    let (fmt, rid) = read_replica(dir)?;
    if fmt != Format::V0 {
        return err("internal: migrate called on a non-v0 repository");
    }
    let op = dir.join("ops.log");
    // 1. Read and validate in memory; the v0 file is not truncated.
    let v0 = store::read_log_with(&op, store::OPLOG_HEADER_V0, false)?;
    let (_store, replica) = crate::replay_records(&op, &v0.records, &|i| i + 2, None)?;
    // 2. ops.log.v1: header + one batch of every v0 record.
    let tmp = dir.join(OPLOG_V1_TMP);
    let mut text = String::with_capacity(v0.records.len() * 120 + 64);
    text.push_str(store::OPLOG_HEADER);
    text.push('\n');
    let mut crcs = Vec::with_capacity(v0.records.len());
    for body in &v0.records {
        let line = store::frame(body);
        crcs.push(line[line.len() - 9..line.len() - 1].to_string());
        text.push_str(&line);
    }
    let mut pos = LogPos::start();
    if !crcs.is_empty() {
        let chain = store::batch_chain(store::CHAIN0, crcs.iter().map(String::as_str));
        let n = crcs.len() as u64;
        text.push_str(&store::frame(&store::commit_body(1, n, &chain)));
        pos = LogPos {
            offset: text.len() as u64,
            seq: 1,
            count: n,
            chain,
            ops: n,
        };
    }
    {
        let mut f = File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        durable::crash_point(durable::MIG_LOG_WRITTEN);
        durable::flush(&f)?;
        durable::barrier(&f)?;
    }
    durable::crash_point(durable::MIG_LOG_SYNCED);
    // 3. Switch: the v1 replica file, atomically.
    let rtmp = dir.join(".replica.tmp");
    {
        let mut f = File::create(&rtmp)?;
        f.write_all(format!("{}\n{rid:032x}\n", store::REPLICA_HEADER).as_bytes())?;
        durable::flush(&f)?;
        durable::crash_point(durable::MIG_REPLICA_TMP);
        durable::barrier(&f)?;
    }
    fs::rename(&rtmp, dir.join("replica"))?;
    dsync(dir)?;
    durable::crash_point(durable::MIG_SWITCHED);
    // 4. Roll forward.
    finish(dir)?;
    // `ops.op_count` must equal what the v1 reader will see.
    debug_assert_eq!(pos.ops as usize, v0.records.len());
    Ok((replica, pos))
}

/// Step 4 (idempotent): move `ops.log.v1` into place, v1 `peers`,
/// `watch.lock`, `format`. Caller holds the exclusive lock, `replica` is v1.
pub fn finish(dir: &Path) -> Result<(), Error> {
    let tmp = dir.join(OPLOG_V1_TMP);
    if tmp.exists() {
        fs::rename(&tmp, dir.join("ops.log"))?;
        dsync(dir)?;
    }
    durable::crash_point(durable::MIG_LOG_RENAMED);
    let peers = dir.join("peers");
    let need = fs::read_to_string(&peers)
        .map(|t| t.lines().next() != Some(watch::PEERS_HEADER))
        .unwrap_or(true);
    if need {
        let list = watch::read_peers(dir)?;
        watch::write_peers(dir, &list)?;
    }
    durable::crash_point(durable::MIG_PEERS);
    if !dir.join(watch::LOCK_FILE).exists() {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(watch::LOCK_FILE))?;
    }
    if fs::read_to_string(dir.join("format")).ok().as_deref() != Some(store::FORMAT_V1) {
        check_format(dir)?;
        store::write_atomic(&dir.join("format"), store::FORMAT_V1.as_bytes())?;
    } else {
        dsync(dir)?;
    }
    durable::crash_point(durable::MIG_FORMAT);
    Ok(())
}
