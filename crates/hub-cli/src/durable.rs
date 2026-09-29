//! Durability primitives and group-commit crash hooks (Issue #29).
//!
//! See Wiki Repository-Semantics, Appendix A.1 [decision-proposal]:
//! - [`flush`]: hand one file's data to the device. Linux/other unix:
//!   `fsync(2)` (durable by itself). macOS: plain `fsync(2)`, which does not
//!   flush the drive cache.
//! - [`barrier`]: make everything already flushed on the volume durable.
//!   macOS: one `fcntl(F_FULLFSYNC)` (fsync, then flush the whole drive
//!   cache, per Apple's fcntl(2)). Elsewhere: `fsync` of that file.
//!   `F_BARRIERFSYNC` is not used: it orders I/O but does not make data
//!   durable before returning.
//!
//! A group commit flushes N files and issues one barrier, instead of one
//! full-fsync per file.

use std::fs::File;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Test-only fault injection: `<stage>[:<k>]` aborts the process at the
/// k-th (default first) time `stage` is reached. Stages: [`BLOBS_WRITTEN`],
/// [`BLOBS_BARRIERED`], [`BLOBS_RENAMED`], [`OPS_APPENDED`], [`OPS_FSYNCED`],
/// [`FILES_WRITTEN`], [`FILES_BARRIERED`], [`FILES_RENAMED`].
#[doc(hidden)]
pub const CRASH_STAGE_ENV: &str = "HUB_TEST_CRASH_STAGE";
/// Blob temps written and flushed, no barrier yet.
pub const BLOBS_WRITTEN: &str = "blobs-written";
/// Barrier over the blob temps done, not renamed yet.
pub const BLOBS_BARRIERED: &str = "blobs-barriered";
/// Temps renamed to blob names, `blobs/` not yet fsynced.
pub const BLOBS_RENAMED: &str = "blobs-renamed";
/// Op lines written to `ops.log`, not yet fsynced.
pub const OPS_APPENDED: &str = "ops-appended";
/// `ops.log` fsynced; later persist steps not done.
pub const OPS_FSYNCED: &str = "ops-fsynced";
/// Op lines and the batch commit record written, not yet fsynced (#33).
pub const COMMIT_WRITTEN: &str = "commit-written";
/// Snapshot temp written and flushed, no barrier/rename yet (#33).
pub const SNAPSHOT_WRITTEN: &str = "snapshot-written";
/// Snapshot renamed into place, `.hub/` not yet fsynced (#33).
pub const SNAPSHOT_RENAMED: &str = "snapshot-renamed";
/// Migration (#33, Appendix A.2.6): `ops.log.v1` written, not synced.
pub const MIG_LOG_WRITTEN: &str = "mig-log-written";
/// Migration: `ops.log.v1` durable, replica still v0.
pub const MIG_LOG_SYNCED: &str = "mig-log-synced";
/// Migration: `.replica.tmp` (v1) written and flushed, not renamed.
pub const MIG_REPLICA_TMP: &str = "mig-replica-tmp";
/// Migration: replica switched to v1 and `.hub/` synced (commit point).
pub const MIG_SWITCHED: &str = "mig-switched";
/// Migration: `ops.log.v1` renamed over `ops.log`.
pub const MIG_LOG_RENAMED: &str = "mig-log-renamed";
/// Migration: `peers` rewritten with the v1 header.
pub const MIG_PEERS: &str = "mig-peers";
/// Migration: `watch.lock` and `format` in place (before the snapshot).
pub const MIG_FORMAT: &str = "mig-format";
/// Checkout: working-file temps written and flushed, no barrier yet.
pub const FILES_WRITTEN: &str = "files-written";
/// Checkout: barrier over the temps done, none renamed yet.
pub const FILES_BARRIERED: &str = "files-barriered";
/// Checkout: temps renamed over their targets, directories not yet
/// barriered, index not saved.
pub const FILES_RENAMED: &str = "files-renamed";

/// Abort if [`CRASH_STAGE_ENV`] names `stage` and this is its k-th hit.
pub fn crash_point(stage: &str) {
    static HITS: AtomicUsize = AtomicUsize::new(0);
    let Some(v) = std::env::var_os(CRASH_STAGE_ENV) else {
        return;
    };
    let v = v.to_string_lossy();
    let (name, k) = match v.split_once(':') {
        Some((n, k)) => (n, k.parse().unwrap_or(1)),
        None => (&*v, 1),
    };
    if name == stage && HITS.fetch_add(1, Ordering::SeqCst) + 1 == k {
        std::process::abort();
    }
}

#[cfg(target_vendor = "apple")]
mod sys {
    use std::os::raw::c_int;
    extern "C" {
        pub fn fsync(fd: c_int) -> c_int;
    }
}

/// Hand `f`'s data and metadata to the device (see module docs).
pub fn flush(f: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::unix::io::AsRawFd;
        loop {
            // SAFETY: valid open fd owned by `f`; fsync has no memory effects.
            if unsafe { sys::fsync(f.as_raw_fd()) } == 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        f.sync_all()
    }
}

/// Test-only: when set, [`flush_dir`] appends each flushed directory path
/// (one per line) to this file.
#[doc(hidden)]
pub const FLUSH_LOG_ENV: &str = "HUB_TEST_FLUSH_LOG";

/// [`flush`] a directory (its entries). On Linux this is a real `fsync` of
/// the directory; on macOS a plain fsync, made durable by the following
/// [`barrier`].
pub fn flush_dir(path: &std::path::Path, f: &File) -> io::Result<()> {
    if let Some(log) = std::env::var_os(FLUSH_LOG_ENV) {
        use std::io::Write;
        let mut l = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        writeln!(l, "{}", path.display())?;
    }
    flush(f)
}

/// Make everything flushed on `f`'s volume durable (see module docs).
/// std's `sync_all` is `F_FULLFSYNC` on Apple targets and `fsync` elsewhere.
pub fn barrier(f: &File) -> io::Result<()> {
    f.sync_all()
}
