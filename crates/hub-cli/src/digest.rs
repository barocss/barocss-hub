//! Issue #46 (Repository-Semantics 4.12, Remote-Sync 5.2.8): a derived,
//! per-author-replica additive digest of an op set, to detect one OpId held
//! with two payloads (a copied `.hub/`) where the id-based delta would
//! never send the op.
//!
//! `h(o)` = SHA-256 of a length-prefixed binary encoding of the op (id,
//! sorted deps, payload). `D_r(S)` = Σ h(o) mod 2^256 over the ops of `S`
//! authored by `r`, with a count. Nothing is stored: sums are computed from
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

/// 256-bit wrapping sum, big-endian bytes.
fn add(acc: &mut [u8; 32], x: &[u8; 32]) {
    let mut carry = 0u16;
    for i in (0..32).rev() {
        let s = acc[i] as u16 + x[i] as u16 + carry;
        acc[i] = s as u8;
        carry = s >> 8;
    }
}

/// Per-author `(count, sum)` of an op set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sums(pub BTreeMap<ReplicaId, (u64, [u8; 32])>);

impl Sums {
    pub fn of<'a>(ops: impl IntoIterator<Item = &'a Op>) -> Sums {
        let mut s = Sums::default();
        for o in ops {
            s.add(o);
        }
        s
    }
    /// [`Sums::of`] hashed on several threads (large sets; one pass of
    /// SHA-256 per op, no per-op allocation).
    pub fn par_of(ops: &[&Op]) -> Sums {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get().min(8))
            .unwrap_or(1);
        if ops.len() < 4096 || threads < 2 {
            return Sums::of(ops.iter().copied());
        }
        let per = ops.len().div_ceil(threads);
        let parts: Vec<Sums> = std::thread::scope(|s| {
            let hs: Vec<_> = ops
                .chunks(per)
                .map(|c| s.spawn(move || Sums::of(c.iter().copied())))
                .collect();
            hs.into_iter()
                .map(|h| h.join().expect("hash thread"))
                .collect()
        });
        let mut out = Sums::default();
        for p in parts {
            for (r, (n, d)) in p.0 {
                let e = out.0.entry(r).or_insert((0, [0; 32]));
                e.0 += n;
                add(&mut e.1, &d);
            }
        }
        out
    }
    pub fn add(&mut self, o: &Op) {
        let e = self.0.entry(o.id.replica).or_insert((0, [0; 32]));
        e.0 += 1;
        add(&mut e.1, &op_hash(o));
    }
    pub fn to_wire(&self) -> Vec<(ReplicaId, u64, [u8; 32])> {
        self.0.iter().map(|(r, (n, d))| (*r, *n, *d)).collect()
    }
    pub fn from_wire(v: &[(ReplicaId, u64, [u8; 32])]) -> Sums {
        Sums(v.iter().map(|(r, n, d)| (*r, (*n, *d))).collect())
    }
    /// Author replicas whose count or sum differ (absent = zero).
    pub fn mismatch(&self, other: &Sums) -> Vec<ReplicaId> {
        let mut keys: Vec<ReplicaId> = self.0.keys().chain(other.0.keys()).copied().collect();
        keys.sort();
        keys.dedup();
        keys.into_iter()
            .filter(|r| self.0.get(r) != other.0.get(r))
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

    #[test]
    fn order_independent_and_content_sensitive() {
        let (a, b, c) = (op(2, 1, "a"), op(3, 1, "b"), op(4, 2, "c"));
        assert_eq!(Sums::of([&a, &b, &c]), Sums::of([&c, &a, &b]));
        let b2 = op(3, 1, "bb");
        let (x, y) = (Sums::of([&a, &b, &c]), Sums::of([&a, &b2, &c]));
        assert_eq!(x.mismatch(&y), vec![ReplicaId(1)]);
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
        assert_ne!(op_hash(&s), op_hash(&s2));
        assert_eq!(
            Sums::of([&a]).mismatch(&Sums::default()),
            vec![ReplicaId(1)]
        );
    }

    #[test]
    fn parallel_equals_direct() {
        let ops: Vec<Op> = (0..9000)
            .map(|i| op(i + 2, 1 + (i % 3) as u128, &format!("n{i}")))
            .collect();
        let refs: Vec<&Op> = ops.iter().collect();
        assert_eq!(Sums::par_of(&refs), Sums::of(ops.iter()));
    }

    #[test]
    fn sum_carries() {
        let mut acc = [0xffu8; 32];
        add(&mut acc, &{
            let mut one = [0u8; 32];
            one[31] = 1;
            one
        });
        assert_eq!(acc, [0u8; 32]);
    }
}
