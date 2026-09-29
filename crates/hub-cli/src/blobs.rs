//! On-disk content-addressed blob store (Issue #19, format v0 [decision-proposal]).
//!
//! Layout: `.hub/blobs/<64 lowercase hex of SHA-256(bytes)>`, raw bytes, no
//! header or compression. Only a file whose name is exactly 64 hex digits is
//! a blob.
//!
//! Writes: bytes go to `.hub/blobs/tmp-<pid>-<nanos>-<seq>` (never a valid
//! blob name), `fsync`, `rename` to the final name, then `fsync` of
//! `.hub/blobs/`. Several blobs are group-committed by [`BlobBatch`]: one
//! barrier and one directory fsync for the whole batch (Issue #29).
//! A crash before the rename leaves only a `tmp-*` file, which is invisible
//! to readers and removed by [`Blobs::sweep_temps`] (called by writers holding
//! the exclusive repo lock).
//!
//! Reads verify SHA-256(bytes) == id; a mismatch is [`BlobError::Corrupt`].
//! [`Blobs::put`] over an existing corrupt blob rewrites it.
//!
//! The empty blob is implicit: [`Blobs::get`] of `BlobId::empty()` succeeds
//! without a file.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use hub_core::tree::BlobId;

use crate::durable;

/// Test-only fault injection: when set, `put` writes half the bytes to the
/// temp file and aborts the process before the rename.
#[doc(hidden)]
pub const CRASH_ENV: &str = "HUB_TEST_CRASH_IN_BLOB_PUT";

#[derive(Debug)]
pub enum BlobError {
    Missing(BlobId),
    Corrupt { id: BlobId, path: PathBuf },
    Io(io::Error),
}

impl std::fmt::Display for BlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlobError::Missing(id) => write!(f, "missing blob {}", id.hex()),
            BlobError::Corrupt { id, path } => write!(
                f,
                "corrupt blob {}: content hash does not match ({})",
                id.hex(),
                path.display()
            ),
            BlobError::Io(e) => write!(f, "i/o error: {e}"),
        }
    }
}

impl From<io::Error> for BlobError {
    fn from(e: io::Error) -> Self {
        BlobError::Io(e)
    }
}

#[derive(Clone, Debug)]
pub struct Blobs {
    dir: PathBuf,
}

fn is_blob_name(n: &str) -> bool {
    n.len() == 64 && n.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl Blobs {
    /// `hub_dir` is `.hub/`; creates `.hub/blobs/` if needed.
    pub fn open(hub_dir: &Path) -> io::Result<Blobs> {
        let dir = hub_dir.join("blobs");
        fs::create_dir_all(&dir)?;
        Ok(Blobs { dir })
    }

    pub fn path(&self, id: &BlobId) -> PathBuf {
        self.dir.join(id.hex())
    }

    /// Write `bytes` to a fresh temp file and make it durable. Not visible
    /// as a blob.
    pub fn write_temp(&self, bytes: &[u8]) -> io::Result<PathBuf> {
        let (tmp, f) = self.write_temp_flushed(bytes)?;
        durable::barrier(&f)?;
        Ok(tmp)
    }

    /// Write `bytes` to a fresh, uniquely named temp file and
    /// [`durable::flush`] it (no barrier: see [`BlobBatch`]).
    fn write_temp_flushed(&self, bytes: &[u8]) -> io::Result<(PathBuf, File)> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp = self.dir.join(format!(
            "tmp-{}-{nanos}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let mut f = File::create(&tmp)?;
        if std::env::var_os(CRASH_ENV).is_some() {
            f.write_all(&bytes[..bytes.len() / 2])?;
            f.sync_all()?;
            std::process::abort();
        }
        f.write_all(bytes)?;
        durable::flush(&f)?;
        Ok((tmp, f))
    }

    /// Store `bytes`; idempotent. Returns their id.
    pub fn put(&self, bytes: &[u8]) -> Result<BlobId, BlobError> {
        let mut b = self.batch();
        let id = b.add(bytes)?;
        b.commit()?;
        Ok(id)
    }

    /// Start a group commit of several blobs (Issue #29).
    pub fn batch(&self) -> BlobBatch<'_> {
        BlobBatch {
            blobs: self,
            temps: Vec::new(),
            seen: BTreeSet::new(),
            last: None,
        }
    }

    /// Read and verify a blob.
    pub fn get(&self, id: &BlobId) -> Result<Vec<u8>, BlobError> {
        if *id == BlobId::empty() {
            return Ok(Vec::new());
        }
        let p = self.path(id);
        let bytes = match fs::read(&p) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(BlobError::Missing(*id)),
            Err(e) => return Err(e.into()),
        };
        if BlobId::of(&bytes) != *id {
            return Err(BlobError::Corrupt { id: *id, path: p });
        }
        Ok(bytes)
    }

    /// True if a verified blob `id` is present.
    pub fn contains(&self, id: &BlobId) -> bool {
        self.get(id).is_ok()
    }

    /// Ids of all visible blob files (unverified).
    pub fn list(&self) -> io::Result<Vec<BlobId>> {
        let mut out = Vec::new();
        for e in fs::read_dir(&self.dir)? {
            let n = e?.file_name();
            let Some(n) = n.to_str() else { continue };
            if is_blob_name(n) {
                let mut b = [0u8; 32];
                for (i, o) in b.iter_mut().enumerate() {
                    *o = u8::from_str_radix(&n[2 * i..2 * i + 2], 16).expect("hex");
                }
                out.push(BlobId(b));
            }
        }
        out.sort();
        Ok(out)
    }

    /// Remove leftover `tmp-*` files from crashed writers. Call only while
    /// holding the exclusive repo lock. Returns how many were removed.
    pub fn sweep_temps(&self) -> io::Result<usize> {
        let mut n = 0;
        for e in fs::read_dir(&self.dir)? {
            let e = e?;
            if e.file_name().to_string_lossy().starts_with("tmp-") {
                fs::remove_file(e.path())?;
                n += 1;
            }
        }
        Ok(n)
    }
}

/// A group commit of blobs (Issue #29, Wiki Appendix A.1): every
/// [`BlobBatch::add`] writes and flushes a temp; [`BlobBatch::commit`] issues
/// one barrier, renames every temp to its blob name and fsyncs `blobs/`
/// once. Until `commit` returns, no new blob is guaranteed visible, so
/// callers append ops naming these blobs only afterwards. Dropping a batch
/// without committing leaves `tmp-*` files for the next sweep.
pub struct BlobBatch<'a> {
    blobs: &'a Blobs,
    temps: Vec<(PathBuf, BlobId)>,
    seen: BTreeSet<BlobId>,
    last: Option<File>,
}

impl BlobBatch<'_> {
    /// Stage `bytes` (skipped if empty, already stored, or already staged).
    pub fn add(&mut self, bytes: &[u8]) -> Result<BlobId, BlobError> {
        let id = BlobId::of(bytes);
        if id == BlobId::empty() || self.seen.contains(&id) {
            return Ok(id);
        }
        match self.blobs.get(&id) {
            Ok(_) => return Ok(id),
            Err(BlobError::Missing(_) | BlobError::Corrupt { .. }) => {}
            Err(e) => return Err(e),
        }
        let (tmp, f) = self.blobs.write_temp_flushed(bytes)?;
        self.temps.push((tmp, id));
        self.seen.insert(id);
        self.last = Some(f);
        Ok(id)
    }

    /// Barrier, renames, one directory fsync. Returns how many blobs were
    /// written.
    pub fn commit(self) -> Result<usize, BlobError> {
        let Some(last) = self.last else {
            return Ok(0);
        };
        durable::crash_point(durable::BLOBS_WRITTEN);
        durable::barrier(&last)?;
        drop(last);
        durable::crash_point(durable::BLOBS_BARRIERED);
        for (tmp, id) in &self.temps {
            fs::rename(tmp, self.blobs.path(id))?;
        }
        durable::crash_point(durable::BLOBS_RENAMED);
        File::open(&self.blobs.dir)?.sync_all()?;
        Ok(self.temps.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn torn_temp_is_invisible_and_corruption_is_detected() {
        let d = std::env::temp_dir().join(format!("hub-blobs-unit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let b = Blobs::open(&d).unwrap();
        let id = BlobId::of(b"abcdef");
        // A crash after writing a partial temp file, before the rename.
        let tmp = b.write_temp(b"abc").unwrap();
        assert!(tmp.exists());
        assert!(matches!(b.get(&id), Err(BlobError::Missing(_))));
        assert!(b.list().unwrap().is_empty());
        assert_eq!(b.sweep_temps().unwrap(), 1);
        assert_eq!(b.put(b"abcdef").unwrap(), id);
        assert_eq!(b.get(&id).unwrap(), b"abcdef");
        assert_eq!(b.list().unwrap(), vec![id]);
        // A torn/corrupt final file is caught on read and repaired by put.
        fs::write(b.path(&id), b"abc").unwrap();
        assert!(matches!(b.get(&id), Err(BlobError::Corrupt { .. })));
        b.put(b"abcdef").unwrap();
        assert_eq!(b.get(&id).unwrap(), b"abcdef");
        assert_eq!(b.get(&BlobId::empty()).unwrap(), b"");
        fs::remove_dir_all(&d).unwrap();
    }
}
