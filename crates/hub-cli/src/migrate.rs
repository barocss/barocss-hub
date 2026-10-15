//! On-disk format migration v0 -> v1 (Issue #33, Wiki Appendix A.2.6).
//!
//! Runs under the exclusive repo lock. Before the switch (the atomic
//! rename of a v1 `replica` file) every v0 file is untouched, so a crash
//! leaves a working v0 repository and the next v1 open starts over. After
//! the switch, [`finish`] rolls the remaining steps forward on every open
//! until they are done.
//!
//! Format v2 (Issue #54, Text-CRDT 7.9, D-053 E-T5): [`upgrade_v2`]
//! switches the `replica` header to v2 (header only, no log rewrite) at
//! the first `TextEdit` appended, and [`finish_v2`] rolls `format` forward.

use crate::{errc, Code};
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
    /// v1 + `T` records + snapshot v2 (Issue #54).
    V2,
}

/// Parse `.hub/replica`: format and replica id.
pub fn read_replica(dir: &Path) -> Result<(Format, u128), Error> {
    let rp = dir.join("replica");
    let rtext = fs::read_to_string(&rp)
        .map_err(|e| Error::new(Code::Io, format!("cannot read {}: {e}", rp.display())))?;
    let lines: Vec<&str> = rtext.lines().collect();
    let fmt = match lines.first() {
        Some(h) if *h == store::REPLICA_HEADER || *h == store::REPLICA_HEADER_RESCUE => Format::V1,
        Some(h) if *h == store::REPLICA_HEADER_V2 || *h == store::REPLICA_HEADER_V2_RESCUE => {
            Format::V2
        }
        Some(h) if *h == store::REPLICA_HEADER_V0 => Format::V0,
        Some(h) if h.starts_with(store::REPLICA_HEADER_PREFIX) => {
            return errc(Code::Corrupt, format!(
                "unsupported repository format '{}' in {} (this hub understands v0, v1 and v2; use a newer hub)",
                &h[store::REPLICA_HEADER_PREFIX.len()..],
                rp.display()
            ))
        }
        _ => return errc(Code::Corrupt, format!("corrupt {}", rp.display())),
    };
    match lines.as_slice() {
        [_, id] if id.len() == 32 => Ok((
            fmt,
            u128::from_str_radix(id, 16)
                .map_err(|_| Error::new(Code::Corrupt, "bad replica id"))?,
        )),
        _ => errc(Code::Corrupt, format!("corrupt {}", rp.display())),
    }
}

/// Roll-forward work left after the switch (a crash mid-migration, or
/// mid v2 upgrade: replica v2, `format` not yet v2).
pub fn needs_finish(dir: &Path) -> bool {
    dir.join(OPLOG_V1_TMP).exists()
        || !dir.join(watch::LOCK_FILE).exists()
        || !dir.join("format").exists()
        || (matches!(read_replica(dir), Ok((Format::V2, _)))
            && fs::read_to_string(dir.join("format")).ok().as_deref() != Some(store::FORMAT_V2))
}

/// Check `.hub/format` if present: v1 under a v1 `replica`; v1 or v2 under
/// a v2 `replica` (v1 = the v2 upgrade's roll-forward is pending).
pub fn check_format_for(dir: &Path, fmt: &Format) -> Result<(), Error> {
    match fs::read_to_string(dir.join("format")) {
        Ok(t) if t == store::FORMAT_V2 && *fmt == Format::V2 => Ok(()),
        Ok(t) if t == store::FORMAT_V2 => errc(
            Code::Corrupt,
            format!(
                "corrupt {}: format v2 under a v1 replica header",
                dir.join("format").display()
            ),
        ),
        _ => check_format(dir),
    }
}

/// Check `.hub/format` if present (must say v1; v2 is checked by
/// [`check_format_for`]).
pub fn check_format(dir: &Path) -> Result<(), Error> {
    match fs::read_to_string(dir.join("format")) {
        Ok(t) if t == store::FORMAT_V1 => Ok(()),
        Ok(t) => errc(Code::Corrupt, format!(
            "unsupported repository format '{}' in {} (this hub understands v0, v1 and v2; use a newer hub)",
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
    // Issue #43 (D-041): `peers` is no longer rewritten here; the exclusive
    // open that follows converts it to `remotes` (remote::migrate_peers).
    durable::crash_point(durable::MIG_PEERS);
    if !dir.join(watch::LOCK_FILE).exists() {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(watch::LOCK_FILE))?;
    }
    let want = match read_replica(dir)?.0 {
        Format::V2 => store::FORMAT_V2,
        _ => store::FORMAT_V1,
    };
    let have = fs::read_to_string(dir.join("format")).ok();
    if have.as_deref() != Some(want) {
        if have.as_deref() != Some(store::FORMAT_V1) {
            check_format(dir)?;
        }
        store::write_atomic(&dir.join("format"), want.as_bytes())?;
        if want == store::FORMAT_V2 {
            durable::crash_point(durable::V2_FORMAT);
        }
    } else {
        dsync(dir)?;
    }
    durable::crash_point(durable::MIG_FORMAT);
    Ok(())
}

/// Format v1 -> v2 (Issue #54, Text-CRDT 7.9): header only, under the
/// exclusive lock, before the first `TextEdit` is written anywhere in
/// `.hub/`. `.replica.tmp` (v2 header, same id, `v2-rescue` if a rescue
/// is in progress) → flush → [`v2-replica-tmp`] → barrier → rename over
/// `replica` → `dsync(.hub/)` → [`v2-switched`] (from here pre-#54
/// binaries refuse the repository) → `format` v2 ([`finish`]) →
/// [`v2-format`]. A crash before the rename leaves a v1 repository (old
/// binaries keep working; the next text append starts over); after it,
/// every open rolls `format` forward. Idempotent.
pub fn upgrade_v2(dir: &Path) -> Result<(), Error> {
    let (fmt, rid) = read_replica(dir)?;
    match fmt {
        Format::V2 => return finish(dir),
        Format::V1 => {}
        Format::V0 => return err("internal: v2 upgrade of a v0 repository"),
    }
    let rescue = crate::rescue::marked(dir);
    let rtmp = dir.join(".replica.tmp");
    {
        let mut f = File::create(&rtmp)?;
        f.write_all(format!("{}\n{rid:032x}\n", store::replica_header(true, rescue)).as_bytes())?;
        durable::flush(&f)?;
        durable::crash_point(durable::V2_REPLICA_TMP);
        durable::barrier(&f)?;
    }
    fs::rename(&rtmp, dir.join("replica"))?;
    dsync(dir)?;
    durable::crash_point(durable::V2_SWITCHED);
    finish(dir)
}
