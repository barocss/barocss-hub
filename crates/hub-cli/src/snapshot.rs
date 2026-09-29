//! `.hub/snapshot` (Issue #33, Wiki Appendix A.2.3): a cache of the
//! replayed replica bound to a committed `ops.log` position. Never trusted
//! blindly: a snapshot that fails any check is ignored (with a warning) and
//! open replays the whole log. Deleting it never changes observable state.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use hub_core::tree::{NodeId, Replica};

use crate::durable;
use crate::store::{LogPos, OPLOG_HEADER};

pub const MAGIC: &[u8] = b"barocss-hub/snapshot/v1\n";
pub const FILE: &str = "snapshot";
pub const TMP: &str = ".snapshot.tmp";

/// Default snapshot policy (Appendix A.2.3) [decision-proposal].
pub const SNAP_MIN: u64 = 4096;
pub const SNAP_DIV: u64 = 16;

#[cfg(test)]
thread_local! {
    /// Test-only policy override for in-process runs: `Some(n)` = SNAP_MIN.
    pub static TEST_MIN: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    /// Test-only: disable snapshots (read and write) in-process.
    pub static TEST_OFF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Snapshots disabled (`HUB_NO_SNAPSHOT=1`, debugging and benchmarks).
pub fn disabled() -> bool {
    #[cfg(test)]
    if TEST_OFF.with(|c| c.get()) {
        return true;
    }
    std::env::var_os("HUB_NO_SNAPSHOT").is_some_and(|v| v != "0" && !v.is_empty())
}

/// `SNAP_MIN`, overridable with `HUB_SNAPSHOT_MIN`.
pub fn snap_min() -> u64 {
    #[cfg(test)]
    if let Some(n) = TEST_MIN.with(|c| c.get()) {
        return n;
    }
    std::env::var("HUB_SNAPSHOT_MIN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(SNAP_MIN)
}

/// Whether a snapshot should be taken: the replayed tail reached
/// `max(SNAP_MIN, covered / SNAP_DIV)` ops. `SNAP_MIN = 0` (tests) means
/// after every write.
pub fn due(covered: u64, total: u64) -> bool {
    let min = snap_min();
    !disabled() && total > covered && (min == 0 || total - covered >= min.max(covered / SNAP_DIV))
}

/// Fast 64-bit checksum (accidental damage only; not cryptographic).
pub fn cksum(b: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ (b.len() as u64);
    let mut ch = b.chunks_exact(8);
    for c in &mut ch {
        let w = u64::from_le_bytes(c.try_into().unwrap());
        h = (h ^ w).wrapping_mul(K).rotate_left(29);
    }
    for &x in ch.remainder() {
        h = (h ^ x as u64).wrapping_mul(K).rotate_left(29);
    }
    h ^= h >> 32;
    h.wrapping_mul(K) ^ (h >> 29)
}

/// Derived values cached next to the replica: the head frontier (in `<`
/// order) and the greatest op counter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    pub frontier: Vec<NodeId>,
    pub max_counter: u64,
}

/// Serialize a snapshot file image.
pub fn encode(replica: &Replica, pos: &LogPos, head: &Head) -> Option<Vec<u8>> {
    let payload = replica.encode_snapshot()?;
    let mut out = Vec::with_capacity(payload.len() + 128);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&pos.offset.to_le_bytes());
    out.extend_from_slice(&pos.seq.to_le_bytes());
    out.extend_from_slice(&pos.count.to_le_bytes());
    out.extend_from_slice(pos.chain.as_bytes());
    out.extend_from_slice(&pos.ops.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&payload);
    out.extend_from_slice(&(head.frontier.len() as u64).to_le_bytes());
    for f in &head.frontier {
        out.extend_from_slice(&f.counter.to_le_bytes());
        out.extend_from_slice(&f.replica.0.to_le_bytes());
    }
    out.extend_from_slice(&head.max_counter.to_le_bytes());
    let c = cksum(&out);
    out.extend_from_slice(&c.to_le_bytes());
    Some(out)
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// Decode and check a snapshot image by itself (no log binding yet).
pub fn decode(b: &[u8]) -> Result<(Replica, LogPos, Head), String> {
    let fixed = MAGIC.len() + 8 * 3 + 16 + 8 * 2;
    if !b.starts_with(MAGIC) {
        return Err("wrong magic or version".into());
    }
    if b.len() < fixed + 8 {
        return Err("truncated".into());
    }
    let body = &b[..b.len() - 8];
    if cksum(body) != u64_at(b, b.len() - 8) {
        return Err("checksum mismatch".into());
    }
    let m = MAGIC.len();
    let chain = std::str::from_utf8(&b[m + 24..m + 40])
        .ok()
        .filter(|c| {
            c.bytes()
                .all(|x| x.is_ascii_hexdigit() && !x.is_ascii_uppercase())
        })
        .ok_or("bad chain")?
        .to_string();
    let pos = LogPos {
        offset: u64_at(b, m),
        seq: u64_at(b, m + 8),
        count: u64_at(b, m + 16),
        chain,
        ops: u64_at(b, m + 40),
    };
    let plen = u64_at(b, m + 48);
    let pend = (plen as usize)
        .checked_add(fixed)
        .filter(|e| *e + 16 <= body.len())
        .ok_or("payload length mismatch")?;
    let replica = Replica::decode_snapshot(&body[fixed..pend])?;
    let nf = u64_at(body, pend) as usize;
    if nf.checked_mul(24).and_then(|x| x.checked_add(pend + 16)) != Some(body.len()) {
        return Err("payload length mismatch".into());
    }
    let mut frontier = Vec::with_capacity(nf);
    for k in 0..nf {
        let at = pend + 8 + 24 * k;
        let r = u128::from_le_bytes(body[at + 8..at + 24].try_into().unwrap());
        frontier.push(NodeId::new(u64_at(body, at), r));
    }
    if !frontier.windows(2).all(|w| w[0] < w[1]) {
        return Err("bad head frontier".into());
    }
    let head = Head {
        frontier,
        max_counter: u64_at(body, body.len() - 8),
    };
    if replica.len() as u64 != pos.ops {
        return Err("op count mismatch".into());
    }
    if pos.seq == 0 || pos.count == 0 || pos.count > pos.ops {
        return Err("bad covered position".into());
    }
    Ok((replica, pos, head))
}

/// Check that `pos`'s commit record ends at `pos.offset` in `ops.log`.
pub fn bound_to_log(log: &Path, pos: &LogPos) -> Result<(), String> {
    let line = pos.commit_line().ok_or("bad covered position")?;
    let len = fs::metadata(log).map_err(|e| e.to_string())?.len();
    let start = pos
        .offset
        .checked_sub(line.len() as u64)
        .filter(|s| *s >= (OPLOG_HEADER.len() + 1) as u64 && pos.offset <= len)
        .ok_or("does not match ops.log (stale)")?;
    let mut f = File::open(log).map_err(|e| e.to_string())?;
    let mut got = vec![0u8; line.len()];
    f.seek(SeekFrom::Start(start))
        .and_then(|_| f.read_exact(&mut got))
        .map_err(|e| e.to_string())?;
    if got != line.as_bytes() {
        return Err("does not match ops.log (stale)".into());
    }
    Ok(())
}

/// Load `.hub/snapshot` if present. `Ok(None)` = none; `Err(reason)` = a
/// snapshot that must be ignored.
pub fn load(dir: &Path) -> Result<Option<(Replica, LogPos, Head)>, String> {
    let bytes = match fs::read(dir.join(FILE)) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let (r, pos, head) = decode(&bytes)?;
    bound_to_log(&dir.join("ops.log"), &pos)?;
    Ok(Some((r, pos, head)))
}

/// Crash-safe write (Appendix A.2.3): temp + flush, barrier, rename, dsync.
pub fn write(dir: &Path, image: &[u8]) -> io::Result<()> {
    let tmp = dir.join(TMP);
    {
        let mut f = File::create(&tmp)?;
        f.write_all(image)?;
        durable::flush(&f)?;
        durable::crash_point(durable::SNAPSHOT_WRITTEN);
        durable::barrier(&f)?;
    }
    fs::rename(&tmp, dir.join(FILE))?;
    durable::crash_point(durable::SNAPSHOT_RENAMED);
    let d = File::open(dir)?;
    durable::flush_dir(dir, &d)?;
    durable::barrier(&d)
}
