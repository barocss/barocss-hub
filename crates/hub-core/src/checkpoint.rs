//! Checkpoints (D-004) and short version ids (D-005), Repository-Semantics
//! §3.4, Issue #13.
//!
//! A [`Checkpoint`] is an inert label on a [`Version`]: metadata beside the op
//! log, never an op. It does not group ops atomically and creating one never
//! changes State, Conflicts or Projection (D-004).
//!
//! A version id ([`VersionId`]) is SHA-256 over a canonical encoding of the
//! canonical frontier (sorted by `<`), so it is defined for any number of
//! frontier OpIds, including the empty (genesis) frontier (D-005). Users see a
//! lowercase hex prefix (default [`SHORT_LEN`] = 8), lengthened when it is
//! ambiguous among the versions a [`VersionIndex`] knows.
//!
//! Encoding (v0, [decision-proposal]):
//! `"barocss-hub/version/v0\n" || u64_be(len F) || for f in F: u64_be(f.counter) || u128_be(f.replica)`.

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

use crate::tree::{Causality, OpId, ReplicaId};
use crate::version::{Store, Version, VersionError};

/// Domain tag of the v0 frontier encoding.
pub const DOMAIN: &[u8] = b"barocss-hub/version/v0\n";
/// Default number of hex digits shown for a short id.
pub const SHORT_LEN: usize = 8;
/// Shortest prefix [`VersionIndex::resolve`] accepts.
pub const MIN_PREFIX: usize = 4;

/// Full 256-bit version id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VersionId(pub [u8; 32]);

impl VersionId {
    /// Id of a canonical frontier. The caller must pass `F(V)` (sorted,
    /// antichain); use [`version_id`] to derive it from a version.
    pub fn of_frontier(frontier: &[OpId]) -> Self {
        let mut h = Sha256::new();
        h.update(DOMAIN);
        h.update((frontier.len() as u64).to_be_bytes());
        for f in frontier {
            h.update(f.counter.to_be_bytes());
            h.update(f.replica.0.to_be_bytes());
        }
        VersionId(h.finalize().into())
    }

    /// 64 lowercase hex digits.
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Default short form: the first [`SHORT_LEN`] hex digits.
    pub fn short(&self) -> String {
        self.hex()[..SHORT_LEN].to_string()
    }
}

/// Id of a version: hash of its canonical frontier.
pub fn version_id(store: &Store, v: &Version) -> VersionId {
    VersionId::of_frontier(&v.frontier(store))
}

/// D-004 checkpoint: `{message, frontier, author, note?}`.
/// `frontier` is always canonical (`F(V)`), so two checkpoints on the same
/// version carry the same frontier and the same id.
///
/// `Debug` and `PartialEq` show / compare the record fields only, never the
/// memo (Issue #32). `Clone` resets the memo: a clone may be handed to
/// another replica (sync), whose store is not the one the memo was
/// computed against, so it recomputes (or fails with `Missing`) there.
pub struct Checkpoint {
    pub message: String,
    frontier: Vec<OpId>,
    pub author: ReplicaId,
    pub note: Option<String>,
    /// Memoized down-closure (Issue #31): computed on the first
    /// [`Checkpoint::version`] call, never at decode. Not part of equality.
    version: OnceLock<Version>,
}

impl PartialEq for Checkpoint {
    fn eq(&self, o: &Self) -> bool {
        self.message == o.message
            && self.frontier == o.frontier
            && self.author == o.author
            && self.note == o.note
    }
}
impl Eq for Checkpoint {}

impl Clone for Checkpoint {
    fn clone(&self) -> Self {
        Checkpoint {
            message: self.message.clone(),
            frontier: self.frontier.clone(),
            author: self.author,
            note: self.note.clone(),
            version: OnceLock::new(),
        }
    }
}

impl std::fmt::Debug for Checkpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Checkpoint")
            .field("message", &self.message)
            .field("frontier", &self.frontier)
            .field("author", &self.author)
            .field("note", &self.note)
            .finish()
    }
}

/// Cheap frontier validation for decoding many checkpoints against one
/// store (Issue #31). Replaces a per-checkpoint `Version::downclose`
/// (O(ops) each, O(checkpoints x ops) at open) with:
/// - one O(ops x deps) scan deciding whether the store is *closed* (every
///   dep present) and *Lamport* (every dep `<` its op, §2.6 #2);
/// - per frontier: every id is in the store (a closed store makes that the
///   only way the down-closure can fail), then canonicalization by `max≺`
///   with [`Causality`] (built once, only if some frontier has 2+ ids). An
///   antichain is returned unchanged.
///
/// A store that is not closed or not Lamport falls back to the eager
/// down-closure, so results (frontier or error) are identical in every case.
pub struct FrontierCheck<'a> {
    store: &'a Store,
    fast: bool,
    causal: OnceCell<Causality>,
}

impl<'a> FrontierCheck<'a> {
    pub fn new(store: &'a Store) -> Self {
        Self::with_fast(store, Self::scan(store))
    }

    /// The one O(ops x deps) scan: the store is closed and Lamport.
    pub fn scan(store: &Store) -> bool {
        store
            .values()
            .all(|o| o.deps.iter().all(|d| *d < o.id && store.contains_key(d)))
    }

    /// A check reusing a [`FrontierCheck::scan`] result computed on this
    /// same store (Issue #32: one scan per command, shared by every decode).
    pub fn with_fast(store: &'a Store, fast: bool) -> Self {
        debug_assert_eq!(fast, Self::scan(store));
        FrontierCheck {
            store,
            fast,
            causal: OnceCell::new(),
        }
    }

    /// The scan result (to share via [`FrontierCheck::with_fast`]).
    pub fn is_fast(&self) -> bool {
        self.fast
    }

    pub fn store(&self) -> &'a Store {
        self.store
    }

    /// Canonical frontier `max≺(ids)` of the version `downclose(ids)`, or the
    /// error `Version::downclose` would give.
    pub fn canonical(&self, ids: &BTreeSet<OpId>) -> Result<Vec<OpId>, VersionError> {
        if !self.fast {
            let v = Version::downclose(self.store, ids.iter().copied())?;
            return Ok(v.frontier(self.store));
        }
        // downclose pops the greatest id first; in a closed store only a
        // frontier id can be missing, so its error names the greatest one.
        if let Some(&m) = ids.iter().rev().find(|i| !self.store.contains_key(i)) {
            return Err(VersionError::Missing(m));
        }
        if ids.len() <= 1 {
            return Ok(ids.iter().copied().collect());
        }
        let c = self.causal.get_or_init(|| Causality::new(self.store));
        Ok(ids
            .iter()
            .copied()
            .filter(|&a| !ids.iter().any(|&b| b != a && c.precedes(a, b)))
            .collect())
    }
}

impl Checkpoint {
    /// Label `v`. Pure: reads the store, changes nothing.
    pub fn new(
        store: &Store,
        v: &Version,
        message: impl Into<String>,
        author: ReplicaId,
        note: Option<String>,
    ) -> Self {
        Checkpoint {
            message: message.into(),
            frontier: v.frontier(store),
            author,
            note,
            version: OnceLock::new(),
        }
    }

    /// Label a version given its canonical frontier `F(V)` (sorted
    /// antichain of ids in the store), e.g. a memoized head frontier (Issue
    /// #32). The caller guarantees canonicity; `id()` hashes it as given.
    pub fn from_canonical(
        frontier: Vec<OpId>,
        message: impl Into<String>,
        author: ReplicaId,
        note: Option<String>,
    ) -> Self {
        Checkpoint {
            message: message.into(),
            frontier,
            author,
            note,
            version: OnceLock::new(),
        }
    }

    /// Label the version whose frontier is (any id set absorbed into) `ids`.
    /// Fails with [`VersionError::Missing`] if an id is unresolvable (§3.4:
    /// such a checkpoint is pending until sync delivers the ops).
    pub fn from_frontier(
        store: &Store,
        ids: impl IntoIterator<Item = OpId>,
        message: impl Into<String>,
        author: ReplicaId,
        note: Option<String>,
    ) -> Result<Self, VersionError> {
        Self::from_frontier_checked(&FrontierCheck::new(store), ids, message, author, note)
    }

    /// [`Checkpoint::from_frontier`] sharing one [`FrontierCheck`] across
    /// many checkpoints (decode at open). Lazy: no down-closure.
    pub fn from_frontier_checked(
        check: &FrontierCheck<'_>,
        ids: impl IntoIterator<Item = OpId>,
        message: impl Into<String>,
        author: ReplicaId,
        note: Option<String>,
    ) -> Result<Self, VersionError> {
        let ids: BTreeSet<OpId> = ids.into_iter().collect();
        Ok(Checkpoint {
            message: message.into(),
            frontier: check.canonical(&ids)?,
            author,
            note,
            version: OnceLock::new(),
        })
    }

    /// Test-only oracle (Issue #31): the pre-#31 eager decode, which
    /// materializes the down-closure of every checkpoint.
    #[cfg(any(test, feature = "oracle"))]
    pub fn from_frontier_eager(
        store: &Store,
        ids: impl IntoIterator<Item = OpId>,
        message: impl Into<String>,
        author: ReplicaId,
        note: Option<String>,
    ) -> Result<Self, VersionError> {
        let ids: BTreeSet<OpId> = ids.into_iter().collect();
        let v = Version::downclose(store, ids.iter().copied())?;
        // (The pre-#31 debug_assert `max_causal == frontier` is dropped: it
        // only holds for Lamport stores, and the oracle must also run on
        // the non-Lamport stores the equivalence test builds.)
        Ok(Checkpoint::new(store, &v, message, author, note))
    }

    pub fn frontier(&self) -> &[OpId] {
        &self.frontier
    }

    /// The labelled version (down-closure of the frontier), computed on
    /// first use and memoized. The store may only have grown since (the
    /// down-closure of a resolvable frontier never changes).
    pub fn version(&self, store: &Store) -> Result<Version, VersionError> {
        if let Some(v) = self.version.get() {
            return Ok(v.clone());
        }
        let v = Version::downclose(store, self.frontier.iter().copied())?;
        Ok(self.version.get_or_init(|| v).clone())
    }

    pub fn id(&self) -> VersionId {
        VersionId::of_frontier(&self.frontier)
    }
}

/// Local checkpoint list (replication is out of scope, §3.4 open question).
/// Insertion-ordered; duplicate messages and multiple labels per version are
/// allowed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checkpoints {
    list: Vec<Checkpoint>,
}

impl Checkpoints {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn add(&mut self, c: Checkpoint) {
        self.list.push(c);
    }
    pub fn iter(&self) -> impl Iterator<Item = &Checkpoint> {
        self.list.iter()
    }
    pub fn len(&self) -> usize {
        self.list.len()
    }
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// Shorter than [`MIN_PREFIX`], longer than 64, or not hex.
    InvalidPrefix,
    NotFound,
    /// Several known versions share the prefix; their full ids, sorted.
    Ambiguous(Vec<VersionId>),
}

/// The versions a short id can name. Every version of a store is not
/// enumerable (exponential), so the index holds: genesis, the principal
/// version `downclose({o})` of every op in the store, and any version added
/// explicitly (checkpoints, current heads, merge results).
///
/// Issue #32: the principal versions (one per op, frontier `[o]`) live in a
/// flat array sorted by id, hashed in parallel with no per-entry
/// allocation; explicit versions stay in a map. Every query treats the two
/// as one set (union by id), so `short` and `resolve` are exactly those of
/// the pre-#32 single map ([`VersionIndex::new_single_map`], test oracle).
/// The principal ids cannot be skipped: a short id's length depends on
/// whether *any* indexed id shares its first 8+ hex digits (D-005).
#[derive(Clone, Debug, Default)]
pub struct VersionIndex {
    by_id: BTreeMap<VersionId, Vec<OpId>>,
    /// `(id of [o], o)` for every op `o`, sorted by id.
    principal: Vec<(VersionId, OpId)>,
}

impl VersionIndex {
    pub fn new(store: &Store) -> Self {
        let mut ix = VersionIndex::default();
        ix.insert_frontier(Vec::new());
        let ids: Vec<OpId> = store.keys().copied().collect();
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(16);
        let chunk = ids.len().div_ceil(threads).max(4096);
        let mut principal: Vec<(VersionId, OpId)> = Vec::with_capacity(ids.len());
        std::thread::scope(|sc| {
            let hs: Vec<_> = ids
                .chunks(chunk)
                .map(|c| {
                    sc.spawn(move || {
                        c.iter()
                            .map(|&o| (VersionId::of_frontier(&[o]), o))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            for h in hs {
                principal.extend(h.join().expect("hash thread"));
            }
        });
        principal.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        principal.dedup_by(|a, b| a.0 == b.0);
        ix.principal = principal;
        ix
    }

    /// Test-only oracle: the pre-#32 index, every version in one map.
    #[cfg(any(test, feature = "oracle"))]
    pub fn new_single_map(store: &Store, cps: &Checkpoints) -> Self {
        let mut ix = VersionIndex::default();
        ix.insert_frontier(Vec::new());
        for &o in store.keys() {
            ix.insert_frontier(vec![o]);
        }
        for c in cps.iter() {
            ix.insert_frontier(c.frontier.clone());
        }
        ix
    }

    /// Index plus every checkpoint's version.
    pub fn with_checkpoints(store: &Store, cps: &Checkpoints) -> Self {
        let mut ix = Self::new(store);
        for c in cps.iter() {
            ix.insert_frontier(c.frontier.clone());
        }
        ix
    }

    pub fn insert(&mut self, store: &Store, v: &Version) -> VersionId {
        self.insert_frontier(v.frontier(store))
    }

    /// Insert a version by its canonical frontier `F(V)` (sorted antichain).
    pub fn insert_frontier(&mut self, f: Vec<OpId>) -> VersionId {
        let id = VersionId::of_frontier(&f);
        if self.principal_get(&id).is_none() {
            self.by_id.entry(id).or_insert(f);
        }
        id
    }

    fn principal_get(&self, id: &VersionId) -> Option<&OpId> {
        self.principal
            .binary_search_by(|p| p.0.cmp(id))
            .ok()
            .map(|i| &self.principal[i].1)
    }

    pub fn len(&self) -> usize {
        self.by_id.len() + self.principal.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Resolve a hex prefix (case-insensitive) to a known version's frontier.
    pub fn resolve(&self, prefix: &str) -> Result<(VersionId, &[OpId]), ResolveError> {
        let p = prefix.to_ascii_lowercase();
        if p.len() < MIN_PREFIX || p.len() > 64 || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ResolveError::InvalidPrefix);
        }
        // Keys with hex prefix `p` are one contiguous range starting at `p`
        // padded with zeros (byte order = hex order), Issue #31.
        let mut lo = [0u8; 32];
        for (i, b) in p.bytes().enumerate() {
            let d = (b as char).to_digit(16).expect("hex checked") as u8;
            lo[i / 2] |= if i % 2 == 0 { d << 4 } else { d };
        }
        let lo = VersionId(lo);
        let mut hits: Vec<(VersionId, &[OpId])> = self
            .by_id
            .range(lo..)
            .take_while(|(id, _)| hex_starts_with(id, &p))
            .map(|(id, f)| (*id, f.as_slice()))
            .collect();
        let start = self.principal.partition_point(|x| x.0 < lo);
        hits.extend(
            self.principal[start..]
                .iter()
                .take_while(|(id, _)| hex_starts_with(id, &p))
                .map(|(id, o)| (*id, std::slice::from_ref(o))),
        );
        // The two parts are disjoint by construction (`insert_frontier`).
        hits.sort_by(|a, b| a.0.cmp(&b.0));
        match hits.as_slice() {
            [] => Err(ResolveError::NotFound),
            [(id, f)] => Ok((*id, f)),
            _ => Err(ResolveError::Ambiguous(hits.iter().map(|h| h.0).collect())),
        }
    }

    /// Resolve and down-close to the version.
    pub fn resolve_version(&self, store: &Store, prefix: &str) -> Result<Version, ResolveError> {
        let (_, f) = self.resolve(prefix)?;
        Ok(Version::downclose(store, f.iter().copied()).expect("indexed ids resolve"))
    }

    /// Shortest unique prefix of `id`, at least [`SHORT_LEN`] digits: the
    /// display form (git-style lengthening when the default is ambiguous).
    pub fn short(&self, id: &VersionId) -> String {
        // The other key sharing the longest hex prefix with `id` is one of
        // its sorted neighbours, so range lookups replace the scan of every
        // key (Issue #31). With two parts (Issue #32), the union's
        // neighbours are among the neighbours in each part. Same result as
        // the pre-#31 loop ([`VersionIndex::short_scan`]).
        let before = self.by_id.range(..*id).next_back().map(|(k, _)| k);
        let after = self
            .by_id
            .range((std::ops::Bound::Excluded(*id), std::ops::Bound::Unbounded))
            .next()
            .map(|(k, _)| k);
        let i = self.principal.partition_point(|x| x.0 < *id);
        let pbefore = i.checked_sub(1).map(|j| &self.principal[j].0);
        let j = if self.principal.get(i).is_some_and(|x| x.0 == *id) {
            i + 1
        } else {
            i
        };
        let pafter = self.principal.get(j).map(|x| &x.0);
        let common = [before, after, pbefore, pafter]
            .into_iter()
            .flatten()
            .map(|k| common_hex_prefix(k, id))
            .max()
            .unwrap_or(0);
        let n = (common + 1).clamp(SHORT_LEN, 64);
        id.hex()[..n].to_string()
    }

    #[cfg(any(test, feature = "oracle"))]
    fn keys(&self) -> impl Iterator<Item = &VersionId> {
        self.by_id.keys().chain(self.principal.iter().map(|x| &x.0))
    }

    /// Test-only oracle: the pre-#31 `short` (hex-formats every key).
    #[cfg(any(test, feature = "oracle"))]
    pub fn short_scan(&self, id: &VersionId) -> String {
        let hex = id.hex();
        let mut n = SHORT_LEN;
        while n < 64
            && self
                .keys()
                .any(|k| k != id && k.hex().starts_with(&hex[..n]))
        {
            n += 1;
        }
        hex[..n].to_string()
    }

    /// Test-only oracle: the pre-#31 `resolve` hit list (full scan).
    #[cfg(any(test, feature = "oracle"))]
    pub fn resolve_scan(&self, prefix: &str) -> Vec<VersionId> {
        let p = prefix.to_ascii_lowercase();
        let mut v: Vec<VersionId> = self
            .keys()
            .filter(|id| id.hex().starts_with(&p))
            .copied()
            .collect();
        v.sort();
        v
    }
}

/// Number of leading hex digits `a` and `b` share (0..=64).
fn common_hex_prefix(a: &VersionId, b: &VersionId) -> usize {
    for i in 0..32 {
        let x = a.0[i] ^ b.0[i];
        if x != 0 {
            return 2 * i + usize::from(x & 0xf0 == 0);
        }
    }
    64
}

/// `id.hex().starts_with(p)` without formatting (`p` lowercase hex).
fn hex_starts_with(id: &VersionId, p: &str) -> bool {
    const D: &[u8; 16] = b"0123456789abcdef";
    p.bytes().enumerate().all(|(i, c)| {
        let b = id.0[i / 2];
        D[usize::from(if i % 2 == 0 { b >> 4 } else { b & 0xf })] == c
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #31: keys sharing 8..64 hex digits (impossible to build from
    /// real SHA-256 ids) lengthen `short` exactly as the pre-#31 scan, and
    /// range `resolve` finds the same hits.
    #[test]
    fn short_and_resolve_with_long_shared_prefixes() {
        let mut ix = VersionIndex::default();
        let base = [0xabu8; 32];
        let mut ids = vec![VersionId(base)];
        for (byte, x) in [
            (4usize, 0x01u8),
            (4, 0x10),
            (9, 0x0f),
            (31, 0x01),
            (0, 0x10),
        ] {
            let mut b = base;
            b[byte] ^= x;
            ids.push(VersionId(b));
        }
        for id in &ids {
            ix.by_id.insert(*id, Vec::new());
        }
        let mut probe = ids.clone();
        let mut a = base; // not in the index
        a[20] ^= 0x40;
        probe.push(VersionId(a));
        for id in &probe {
            assert_eq!(ix.short(id), ix.short_scan(id), "{}", id.hex());
            for n in 4..=64 {
                let p = &id.hex()[..n];
                let hits = match ix.resolve(p) {
                    Ok((i, _)) => vec![i],
                    Err(ResolveError::Ambiguous(v)) => v,
                    Err(_) => vec![],
                };
                assert_eq!(hits, ix.resolve_scan(p), "{p}");
            }
        }
        assert_eq!(ix.short(&ids[0]).len(), 64);
        assert_eq!(ix.short(&ids[3]).len(), 20);
    }

    /// Issue #32: the same long-shared-prefix keys split between the
    /// principal array and the explicit map (every split mask) give the
    /// same `short` and `resolve` (ids and frontiers) as one map.
    #[test]
    fn split_index_equals_single_map() {
        let base = [0xabu8; 32];
        let mut ids = vec![VersionId(base)];
        for (byte, x) in [
            (4usize, 0x01u8),
            (4, 0x10),
            (9, 0x0f),
            (31, 0x01),
            (0, 0x10),
            (3, 0x01),
        ] {
            let mut b = base;
            b[byte] ^= x;
            ids.push(VersionId(b));
        }
        let op = |i: usize| OpId::new(i as u64 + 2, 1);
        let mut one = VersionIndex::default();
        for (i, id) in ids.iter().enumerate() {
            one.by_id.insert(*id, vec![op(i)]);
        }
        let mut probe = ids.clone();
        let mut a = base;
        a[20] ^= 0x40;
        probe.push(VersionId(a));
        for mask in 0u32..(1 << ids.len()) {
            let mut two = VersionIndex::default();
            for (i, id) in ids.iter().enumerate() {
                if mask & (1 << i) != 0 {
                    two.principal.push((*id, op(i)));
                } else {
                    two.by_id.insert(*id, vec![op(i)]);
                }
            }
            two.principal.sort();
            assert_eq!(two.len(), one.len());
            for id in &probe {
                assert_eq!(two.short(id), one.short(id), "mask {mask:b} {}", id.hex());
                for n in 4..=64 {
                    let p = &id.hex()[..n];
                    let r1 = one.resolve(p).map(|(i, f)| (i, f.to_vec()));
                    let r2 = two.resolve(p).map(|(i, f)| (i, f.to_vec()));
                    assert_eq!(r2, r1, "mask {mask:b} {p}");
                }
            }
        }
    }
}
