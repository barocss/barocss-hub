//! Issue #46 (Repository-Semantics 4.12, Remote-Sync 5.2.8): a derived,
//! per-author-replica additive digest of an op set, to detect one OpId held
//! with two payloads (a copied `.hub/`) where the id-based delta would
//! never send the op.
//!
//! `h(o)` = SHA-256 of a length-prefixed binary encoding of the op (id,
//! sorted deps, payload). `D_r(S)` = LtHash16 (1024 lanes of u16, lane-wise
//! sum mod 2^16) of the BLAKE3-XOF elements of `h(o)` over the ops of `S`
//! authored by `r`, with a count (#46 Review R2: a plain sum mod 2^256 falls
//! to Wagner's generalized birthday attack). The wire carries a 32-byte
//! BLAKE3 checksum of each value. Nothing is stored: sums are computed from
//! the loaded store per session, so there is no format change.

use std::collections::BTreeMap;

use hub_core::tree::{Kind, NodeId, Op, Payload, ReplicaId};
use sha2::{Digest, Sha256};

fn id(h: &mut Sha256, n: NodeId) {
    h.update(n.counter.to_be_bytes());
    h.update(n.replica.0.to_be_bytes());
}

fn name(h: &mut Sha256, s: &str) {
    h.update((s.len() as u64).to_be_bytes());
    h.update(s.as_bytes());
}

/// The canonical content hash of one op.
pub fn op_hash(o: &Op) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"bhub-op-v1");
    id(&mut h, o.id);
    h.update((o.deps.len() as u64).to_be_bytes());
    for d in &o.deps {
        id(&mut h, *d);
    }
    match &o.payload {
        Payload::Create {
            kind,
            parent,
            name: n,
        } => {
            h.update([
                0,
                match kind {
                    Kind::Root => 0,
                    Kind::Directory => 1,
                    Kind::File => 2,
                },
            ]);
            id(&mut h, *parent);
            name(&mut h, n);
        }
        Payload::Move {
            node,
            new_parent,
            new_name,
        } => {
            h.update([1]);
            id(&mut h, *node);
            id(&mut h, *new_parent);
            name(&mut h, new_name);
        }
        Payload::Delete { node } => {
            h.update([2]);
            id(&mut h, *node);
        }
        Payload::SetContent { node, blob } => {
            h.update([3]);
            id(&mut h, *node);
            h.update(blob.0);
        }
    }
    h.finalize().into()
}

/// LtHash16 lanes (Lewi et al., IACR ePrint 2019/227): n = 1024, d = 16.
pub const LANES: usize = 1024;
const XOF_CONTEXT: &str = "barocss-hub 2026-10 lthash16 op v1";

/// One LtHash16 value: 1024 lanes of u16, combined by lane-wise addition
/// mod 2^16. Additive, so `D(A ∪ B) = D(A) + D(B)` for disjoint sets, and
/// order-independent.
#[derive(Clone, PartialEq, Eq)]
pub struct Lt(Box<[u16; LANES]>);

impl std::fmt::Debug for Lt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Lt({})", hex(&self.checksum()))
    }
}

impl Default for Lt {
    fn default() -> Self {
        Lt(Box::new([0; LANES]))
    }
}

fn hex(d: &[u8; 32]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

impl Lt {
    /// The element of one op: 2 KiB of BLAKE3 XOF output (derive-key mode,
    /// domain-separated) over the op's SHA-256 content hash.
    pub fn of_op(o: &Op) -> Lt {
        let mut l = Lt::default();
        l.add_op(o);
        l
    }
    /// `self += element(o)` without allocating.
    pub fn add_op(&mut self, o: &Op) {
        let mut h = blake3::Hasher::new_derive_key(XOF_CONTEXT);
        h.update(&op_hash(o));
        let mut bytes = [0u8; 2 * LANES];
        h.finalize_xof().fill(&mut bytes);
        for (a, c) in self.0.iter_mut().zip(bytes.chunks_exact(2)) {
            *a = a.wrapping_add(u16::from_le_bytes([c[0], c[1]]));
        }
    }
    pub fn add(&mut self, x: &Lt) {
        for (a, b) in self.0.iter_mut().zip(x.0.iter()) {
            *a = a.wrapping_add(*b);
        }
    }
    pub fn sub(&mut self, x: &Lt) {
        for (a, b) in self.0.iter_mut().zip(x.0.iter()) {
            *a = a.wrapping_sub(*b);
        }
    }
    /// 32-byte BLAKE3 checksum of the 2 KiB value (what the wire carries:
    /// equal checksums mean equal LtHashes unless BLAKE3 collides).
    pub fn checksum(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new_derive_key("barocss-hub 2026-10 lthash16 checksum v1");
        for v in self.0.iter() {
            h.update(&v.to_le_bytes());
        }
        *h.finalize().as_bytes()
    }
}

/// Ops per chunk of the sequence digest (positions in `K`'s `<` order).
pub const CHUNK: usize = 16_384;

/// The full-replica wire digest (#46 Review R2): per author, a SHA-256
/// commitment to the `<`-ordered op hashes of that author's ops in `K`,
/// computed in fixed position chunks of [`CHUNK`] (so it parallelizes and
/// is a pure function of `K`). Collision resistant (SHA-256), so a chosen
/// set cannot match another (no Wagner-style attack). Not additive: it is
/// recomputed from `K` (one SHA-256 per op).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sums(pub BTreeMap<ReplicaId, (u64, [u8; 32])>);

type Chunk = BTreeMap<ReplicaId, (u64, Sha256)>;

fn chunk_of(ops: &[&Op]) -> BTreeMap<ReplicaId, (u64, [u8; 32])> {
    let mut m: Chunk = BTreeMap::new();
    for o in ops {
        let e = m.entry(o.id.replica).or_insert_with(|| (0, Sha256::new()));
        e.0 += 1;
        e.1.update(op_hash(o));
    }
    m.into_iter()
        .map(|(r, (n, h))| (r, (n, h.finalize().into())))
        .collect()
}

impl Sums {
    /// `ops` must be in `<` order (a store's value order, or a filtered
    /// subsequence of it); equal sets then give equal digests.
    pub fn of_sorted(ops: &[&Op]) -> Sums {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get().min(16))
            .unwrap_or(1);
        let chunks: Vec<&[&Op]> = ops.chunks(CHUNK).collect();
        let parts: Vec<BTreeMap<ReplicaId, (u64, [u8; 32])>> = if chunks.len() < 2 || threads < 2 {
            chunks.iter().map(|c| chunk_of(c)).collect()
        } else {
            let per = chunks.len().div_ceil(threads);
            std::thread::scope(|s| {
                let hs: Vec<_> = chunks
                    .chunks(per)
                    .map(|g| s.spawn(move || g.iter().map(|c| chunk_of(c)).collect::<Vec<_>>()))
                    .collect();
                hs.into_iter()
                    .flat_map(|h| h.join().expect("hash thread"))
                    .collect()
            })
        };
        let mut fin: BTreeMap<ReplicaId, (u64, Sha256)> = BTreeMap::new();
        for (ci, part) in parts.iter().enumerate() {
            for (r, (n, d)) in part {
                let e = fin.entry(*r).or_insert_with(|| {
                    let mut h = Sha256::new();
                    h.update(b"bhub-seq-v1");
                    (0, h)
                });
                e.0 += n;
                e.1.update((ci as u64).to_be_bytes());
                e.1.update(n.to_be_bytes());
                e.1.update(d);
            }
        }
        Sums(
            fin.into_iter()
                .map(|(r, (n, h))| (r, (n, h.finalize().into())))
                .collect(),
        )
    }
    pub fn to_wire(&self) -> Vec<(ReplicaId, u64, [u8; 32])> {
        self.0.iter().map(|(r, (n, d))| (*r, *n, *d)).collect()
    }
    /// Author replicas whose count or digest differ from the peer's lines.
    pub fn mismatch(&self, theirs: &[(ReplicaId, u64, [u8; 32])]) -> Vec<ReplicaId> {
        let theirs: BTreeMap<ReplicaId, (u64, [u8; 32])> =
            theirs.iter().map(|(r, n, d)| (*r, (*n, *d))).collect();
        let mut keys: Vec<ReplicaId> = self.0.keys().chain(theirs.keys()).copied().collect();
        keys.sort();
        keys.dedup();
        keys.into_iter()
            .filter(|r| self.0.get(r) != theirs.get(r))
            .collect()
    }
}

/// Per-author `(count, LtHash16)` of an op set: the additive digest for
/// partial replicas (#47: `D_r(B) + D_r(W)`). Not on the full-replica wire
/// path (cost, see [`Sums`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LtSums(pub BTreeMap<ReplicaId, (u64, Lt)>);

impl LtSums {
    pub fn of<'a>(ops: impl IntoIterator<Item = &'a Op>) -> LtSums {
        let mut s = LtSums::default();
        for o in ops {
            s.add(o);
        }
        s
    }
    /// [`LtSums::of`] on several threads (large sets).
    pub fn par_of(ops: &[&Op]) -> LtSums {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get().min(16))
            .unwrap_or(1);
        if ops.len() < 2048 || threads < 2 {
            return LtSums::of(ops.iter().copied());
        }
        let per = ops.len().div_ceil(threads);
        let parts: Vec<LtSums> = std::thread::scope(|s| {
            let hs: Vec<_> = ops
                .chunks(per)
                .map(|c| s.spawn(move || LtSums::of(c.iter().copied())))
                .collect();
            hs.into_iter()
                .map(|h| h.join().expect("hash thread"))
                .collect()
        });
        let mut out = LtSums::default();
        for p in parts {
            out.merge(p);
        }
        out
    }
    /// Add a disjoint set's sums (the partial-replica `D(B) + D(W)`).
    pub fn merge(&mut self, p: LtSums) {
        for (r, (n, d)) in p.0 {
            let e = self.0.entry(r).or_default();
            e.0 += n;
            e.1.add(&d);
        }
    }
    /// Remove a subset's sums (`D(S \ T) = D(S) - D(T)` for `T ⊆ S`).
    pub fn minus(&mut self, t: &LtSums) {
        for (r, (n, d)) in &t.0 {
            let e = self.0.entry(*r).or_default();
            e.0 -= n;
            e.1.sub(d);
            if e.0 == 0 {
                self.0.remove(r);
            }
        }
    }
    pub fn add(&mut self, o: &Op) {
        let e = self.0.entry(o.id.replica).or_default();
        e.0 += 1;
        e.1.add_op(o);
    }
    /// `(replica, count, checksum)` lines of the `Sums` frame.
    pub fn to_wire(&self) -> Vec<(ReplicaId, u64, [u8; 32])> {
        self.0
            .iter()
            .map(|(r, (n, d))| (*r, *n, d.checksum()))
            .collect()
    }
    /// Author replicas whose count or checksum differ from the peer's
    /// `Sums` lines (absent = empty set).
    pub fn mismatch(&self, theirs: &[(ReplicaId, u64, [u8; 32])]) -> Vec<ReplicaId> {
        let empty = Lt::default().checksum();
        let ours: BTreeMap<ReplicaId, (u64, [u8; 32])> = self
            .to_wire()
            .into_iter()
            .map(|(r, n, d)| (r, (n, d)))
            .collect();
        let theirs: BTreeMap<ReplicaId, (u64, [u8; 32])> =
            theirs.iter().map(|(r, n, d)| (*r, (*n, *d))).collect();
        let mut keys: Vec<ReplicaId> = ours.keys().chain(theirs.keys()).copied().collect();
        keys.sort();
        keys.dedup();
        keys.into_iter()
            .filter(|r| {
                ours.get(r).copied().unwrap_or((0, empty))
                    != theirs.get(r).copied().unwrap_or((0, empty))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hub_core::tree::{BlobId, ROOT};
    use std::collections::BTreeSet;

    fn op(c: u64, r: u128, name: &str) -> Op {
        Op {
            id: NodeId::new(c, r),
            deps: BTreeSet::new(),
            payload: Payload::Create {
                kind: Kind::File,
                parent: ROOT,
                name: name.into(),
            },
        }
    }

    fn wire(s: &LtSums) -> Vec<(ReplicaId, u64, [u8; 32])> {
        s.to_wire()
    }

    #[test]
    fn order_independent_and_content_sensitive() {
        let (a, b, c) = (op(2, 1, "a"), op(3, 1, "b"), op(4, 2, "c"));
        assert_eq!(LtSums::of([&a, &b, &c]), LtSums::of([&c, &a, &b]));
        let b2 = op(3, 1, "bb");
        let (x, y) = (LtSums::of([&a, &b, &c]), LtSums::of([&a, &b2, &c]));
        assert_eq!(x.mismatch(&wire(&y)), vec![ReplicaId(1)]);
        assert!(x.mismatch(&wire(&x)).is_empty());
        let mut s = op(5, 1, "");
        s.payload = Payload::SetContent {
            node: a.id,
            blob: BlobId::of(b"A"),
        };
        let mut s2 = s.clone();
        s2.payload = Payload::SetContent {
            node: a.id,
            blob: BlobId::of(b"BB"),
        };
        assert_ne!(Lt::of_op(&s), Lt::of_op(&s2));
        assert_eq!(LtSums::of([&a]).mismatch(&[]), vec![ReplicaId(1)]);
        assert!(LtSums::default().mismatch(&[]).is_empty());
    }

    #[test]
    fn additive_and_subtractive() {
        let ops: Vec<Op> = (0..50).map(|i| op(i + 2, 1, &format!("n{i}"))).collect();
        let (lo, hi) = ops.split_at(20);
        let mut ab = LtSums::of(lo.iter());
        ab.merge(LtSums::of(hi.iter()));
        assert_eq!(ab, LtSums::of(ops.iter()));
        // D(B ∪ W) - D(W) = D(B) (thin client over a server base).
        let mut l = LtSums::of(ops.iter()).0[&ReplicaId(1)].1.clone();
        l.sub(&LtSums::of(hi.iter()).0[&ReplicaId(1)].1);
        assert_eq!(l, LtSums::of(lo.iter()).0[&ReplicaId(1)].1);
        let mut m = LtSums::of(ops.iter());
        m.minus(&LtSums::of(hi.iter()));
        assert_eq!(m, LtSums::of(lo.iter()));
        m.minus(&LtSums::of(lo.iter()));
        assert_eq!(m, LtSums::default());
    }

    fn seq(ops: &[Op]) -> Sums {
        let mut v: Vec<&Op> = ops.iter().collect();
        v.sort_by_key(|o| o.id);
        Sums::of_sorted(&v)
    }

    #[test]
    fn sequence_digest_detects_single_op_changes_and_is_chunk_stable() {
        let ops: Vec<Op> = (0..40_000)
            .map(|i| op(i + 2, 1 + (i % 5) as u128, &format!("n{i}")))
            .collect();
        let base = seq(&ops);
        assert_eq!(base, seq(&ops));
        assert!(base.mismatch(&base.to_wire()).is_empty());
        for i in [0usize, 16_383, 16_384, 39_999] {
            let mut v = ops.clone();
            v[i].payload = Payload::Delete { node: ROOT };
            assert_eq!(
                base.mismatch(&seq(&v).to_wire()),
                vec![ops[i].id.replica],
                "op {i}"
            );
        }
        // A missing op changes its author's count (later chunks shift, so
        // other authors may differ too: K differs, never a false positive).
        let r = base.mismatch(&seq(&ops[1..]).to_wire());
        assert!(r.contains(&ops[0].id.replica), "{r:?}");
        // Single-threaded chunks equal the parallel result.
        let v: Vec<&Op> = ops.iter().collect();
        let one: Vec<_> = v.chunks(CHUNK).map(chunk_of).collect();
        assert_eq!(one.len(), 3);
        assert_eq!(Sums::of_sorted(&v), base);
    }

    #[test]
    fn every_single_op_change_is_detected() {
        let ops: Vec<Op> = (0..200)
            .map(|i| op(i + 2, 1 + (i % 4) as u128, &format!("n{i}")))
            .collect();
        let base = LtSums::of(ops.iter());
        for i in 0..ops.len() {
            let mut v = ops.clone();
            v[i].payload = Payload::Delete { node: ROOT };
            let r = base.mismatch(&wire(&LtSums::of(v.iter())));
            assert_eq!(r, vec![ops[i].id.replica], "op {i}");
        }
    }

    /// Release-only timing probe (`cargo test --release -- --ignored`).
    #[test]
    #[ignore]
    fn timing_probe() {
        let ops: Vec<Op> = (0..200_000)
            .map(|i| op(i + 2, 1, &format!("n{i}")))
            .collect();
        let refs: Vec<&Op> = ops.iter().collect();
        let t = std::time::Instant::now();
        let s = LtSums::par_of(&refs);
        eprintln!("200k ops: {:?} ({:?})", t.elapsed(), s.0.len());
        let t = std::time::Instant::now();
        let _ = LtSums::of(refs.iter().copied());
        eprintln!("200k ops single-thread: {:?}", t.elapsed());
    }

    #[test]
    fn parallel_equals_direct() {
        let ops: Vec<Op> = (0..5000)
            .map(|i| op(i + 2, 1 + (i % 3) as u128, &format!("n{i}")))
            .collect();
        let refs: Vec<&Op> = ops.iter().collect();
        assert_eq!(LtSums::par_of(&refs), LtSums::of(ops.iter()));
    }
}
