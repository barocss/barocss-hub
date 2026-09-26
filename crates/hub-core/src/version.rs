//! Version model (Repository-Semantics §3.1-§3.3, Issue #12).
//!
//! A [`Version`] is a causally closed op set, stored as its OpIds. Everything
//! that needs deps (closure checks, frontier, down-closure) takes the op
//! store (`OpId -> Op`) explicitly: a version is meaningful only relative to a
//! store that can resolve its ids (§3.1). Checkpoints / user commits (§3.4)
//! live in `checkpoint`.
//!
//! Identity is the frontier (§3.1). The version vector is a derived summary;
//! it determines the version only when the store satisfies §2.6 #7
//! (own-chain deps). This module does not reject ops that violate §2.6 #7;
//! [`own_chain_ok`] detects such a store, and [`Version::from_vv`] /
//! [`compare_vv`] are only meaningful when it holds (Issue #12 choice: VV is
//! a hint, the frontier and the op set are authoritative).

use std::collections::{BTreeMap, BTreeSet};

use crate::tree::{Op, OpId, ReplicaId};

/// Op store: every op a replica can resolve, keyed by id.
pub type Store = BTreeMap<OpId, Op>;

/// Version vector `ReplicaId -> max Counter` (absent = 0).
pub type VersionVector = BTreeMap<ReplicaId, u64>;

/// Result of comparing two versions by op-set inclusion (§3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    Equal,
    /// Left is a strict subset of right.
    Ancestor,
    /// Left is a strict superset of right.
    Descendant,
    Concurrent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VersionError {
    /// An OpId (in a frontier, a set, or some op's deps) is not in the store.
    Missing(OpId),
    /// The op set is not causally closed: `op` is present, `dep` is not.
    NotClosed { op: OpId, dep: OpId },
}

/// A causally closed op set (§3.1). Invariant: closed w.r.t. the store it
/// was built from; constructors check it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Version {
    ops: BTreeSet<OpId>,
}

/// `a ≺ b`, i.e. `a ∈ deps*(b)`, by walking deps from `b`.
pub fn causally_below(store: &Store, a: OpId, b: OpId) -> bool {
    // Lamport consistency (§2.6 #2): deps are `<` their op, so prune below a.
    let mut stack = vec![b];
    let mut seen = BTreeSet::new();
    while let Some(x) = stack.pop() {
        let Some(op) = store.get(&x) else { continue };
        for &d in &op.deps {
            if d == a {
                return true;
            }
            if d > a && seen.insert(d) {
                stack.push(d);
            }
        }
    }
    false
}

/// `max≺` of a set of ids: drop every id that is causally below another.
/// Result sorted by `<` (canonical frontier form, §3.1).
pub fn max_causal(store: &Store, ids: &BTreeSet<OpId>) -> Vec<OpId> {
    ids.iter()
        .copied()
        .filter(|&a| !ids.iter().any(|&b| b != a && causally_below(store, a, b)))
        .collect()
}

impl Version {
    /// The empty version (state `S0`).
    pub fn genesis() -> Self {
        Version::default()
    }

    /// Build from an explicit op set, checking that it is causally closed.
    pub fn from_ops(store: &Store, ops: BTreeSet<OpId>) -> Result<Self, VersionError> {
        for &id in &ops {
            let op = store.get(&id).ok_or(VersionError::Missing(id))?;
            for &d in &op.deps {
                if !ops.contains(&d) {
                    return Err(VersionError::NotClosed { op: id, dep: d });
                }
            }
        }
        Ok(Version { ops })
    }

    /// Down-closure of a frontier (§3.1): `F ∪ ⋃ deps*(f)`.
    /// Any id set is accepted (non-maximal members are simply absorbed).
    pub fn downclose<I: IntoIterator<Item = OpId>>(
        store: &Store,
        frontier: I,
    ) -> Result<Self, VersionError> {
        let mut ops = BTreeSet::new();
        let mut stack: Vec<OpId> = frontier.into_iter().collect();
        while let Some(id) = stack.pop() {
            if !ops.insert(id) {
                continue;
            }
            let op = store.get(&id).ok_or(VersionError::Missing(id))?;
            stack.extend(op.deps.iter().copied().filter(|d| !ops.contains(d)));
        }
        Ok(Version { ops })
    }

    /// Version from a version vector: for each replica `r`, all ops of `r`
    /// in the store with `Counter <= vv[r]`. Equals the version with that VV
    /// only if the store satisfies §2.6 #7; the result is checked for closure.
    pub fn from_vv(store: &Store, vv: &VersionVector) -> Result<Self, VersionError> {
        let ops = store
            .keys()
            .copied()
            .filter(|id| vv.get(&id.replica).is_some_and(|&c| id.counter <= c))
            .collect();
        Version::from_ops(store, ops)
    }

    pub fn ops(&self) -> &BTreeSet<OpId> {
        &self.ops
    }

    pub fn contains(&self, id: OpId) -> bool {
        self.ops.contains(&id)
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Canonical frontier `F(V)`: maximal ops, sorted by `<` (§3.1).
    /// An op is non-maximal iff it is a direct dep of some op in `V`
    /// (closure makes direct deps sufficient).
    pub fn frontier(&self, store: &Store) -> Vec<OpId> {
        let mut f = self.ops.clone();
        for id in &self.ops {
            for d in &store[id].deps {
                f.remove(d);
            }
        }
        f.into_iter().collect()
    }

    /// `VV(V)[r] = max{c | (c, r) ∈ V}` (§3.1).
    pub fn vv(&self) -> VersionVector {
        let mut vv = VersionVector::new();
        for id in &self.ops {
            let e = vv.entry(id.replica).or_insert(0);
            *e = (*e).max(id.counter);
        }
        vv
    }

    /// ⊔ = union (§3.2); closed because both inputs are.
    pub fn merge(&self, other: &Version) -> Version {
        Version {
            ops: self.ops.union(&other.ops).copied().collect(),
        }
    }

    /// ⊓ = intersection, the greatest common ancestor (§3.2).
    pub fn meet(&self, other: &Version) -> Version {
        Version {
            ops: self.ops.intersection(&other.ops).copied().collect(),
        }
    }

    /// Ops of `self` in `<` order, e.g. for `tree::evaluate` (§3.3).
    pub fn resolve<'a>(&'a self, store: &'a Store) -> impl Iterator<Item = &'a Op> + 'a {
        self.ops.iter().map(move |id| &store[id])
    }
}

/// Compare by op-set inclusion (§3.2).
pub fn compare(a: &Version, b: &Version) -> Relation {
    relation(a.ops.is_subset(&b.ops), b.ops.is_subset(&a.ops))
}

/// Compare via frontiers only (§3.2): `a ⊆ b` iff every `f ∈ F(a)` is in `b`.
pub fn compare_frontier(store: &Store, a: &Version, b: &Version) -> Relation {
    let a_in_b = a.frontier(store).iter().all(|f| b.contains(*f));
    let b_in_a = b.frontier(store).iter().all(|f| a.contains(*f));
    relation(a_in_b, b_in_a)
}

/// Compare via version vectors (pointwise ≤). Agrees with [`compare`] only
/// under §2.6 #7.
pub fn compare_vv(a: &VersionVector, b: &VersionVector) -> Relation {
    let le = |x: &VersionVector, y: &VersionVector| {
        x.iter().all(|(r, c)| y.get(r).copied().unwrap_or(0) >= *c)
    };
    relation(le(a, b), le(b, a))
}

fn relation(a_le_b: bool, b_le_a: bool) -> Relation {
    match (a_le_b, b_le_a) {
        (true, true) => Relation::Equal,
        (true, false) => Relation::Ancestor,
        (false, true) => Relation::Descendant,
        (false, false) => Relation::Concurrent,
    }
}

/// Merge frontier from frontiers alone (§3.2, §3.5 #5): `max≺(F1 ∪ F2)`.
pub fn merge_frontiers(store: &Store, f1: &[OpId], f2: &[OpId]) -> Vec<OpId> {
    let ids: BTreeSet<OpId> = f1.iter().chain(f2).copied().collect();
    max_causal(store, &ids)
}

/// §2.6 #7 check: every op of replica `r` has `r`'s previous op (by counter)
/// in its causal past.
pub fn own_chain_ok(store: &Store) -> bool {
    let mut last: BTreeMap<ReplicaId, OpId> = BTreeMap::new();
    // Ascending `<` visits each replica's ops in counter order.
    for id in store.keys() {
        if let Some(&prev) = last.get(&id.replica) {
            if !causally_below(store, prev, *id) {
                return false;
            }
        }
        last.insert(id.replica, *id);
    }
    true
}
