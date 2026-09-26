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

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use crate::tree::{OpId, ReplicaId};
use crate::version::{max_causal, Store, Version, VersionError};

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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    pub message: String,
    frontier: Vec<OpId>,
    pub author: ReplicaId,
    pub note: Option<String>,
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
        let ids: BTreeSet<OpId> = ids.into_iter().collect();
        let v = Version::downclose(store, ids.iter().copied())?;
        debug_assert_eq!(max_causal(store, &ids), v.frontier(store));
        Ok(Checkpoint::new(store, &v, message, author, note))
    }

    pub fn frontier(&self) -> &[OpId] {
        &self.frontier
    }

    /// The labelled version (down-closure of the frontier).
    pub fn version(&self, store: &Store) -> Result<Version, VersionError> {
        Version::downclose(store, self.frontier.iter().copied())
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
#[derive(Clone, Debug, Default)]
pub struct VersionIndex {
    by_id: BTreeMap<VersionId, Vec<OpId>>,
}

impl VersionIndex {
    pub fn new(store: &Store) -> Self {
        let mut ix = VersionIndex::default();
        ix.insert_frontier(Vec::new());
        for &o in store.keys() {
            ix.insert_frontier(vec![o]);
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

    fn insert_frontier(&mut self, f: Vec<OpId>) -> VersionId {
        let id = VersionId::of_frontier(&f);
        self.by_id.entry(id).or_insert(f);
        id
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Resolve a hex prefix (case-insensitive) to a known version's frontier.
    pub fn resolve(&self, prefix: &str) -> Result<(VersionId, &[OpId]), ResolveError> {
        let p = prefix.to_ascii_lowercase();
        if p.len() < MIN_PREFIX || p.len() > 64 || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ResolveError::InvalidPrefix);
        }
        let hits: Vec<_> = self
            .by_id
            .iter()
            .filter(|(id, _)| id.hex().starts_with(&p))
            .collect();
        match hits.as_slice() {
            [] => Err(ResolveError::NotFound),
            [(id, f)] => Ok((**id, f.as_slice())),
            _ => Err(ResolveError::Ambiguous(hits.iter().map(|h| *h.0).collect())),
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
        let hex = id.hex();
        let mut n = SHORT_LEN;
        while n < 64
            && self
                .by_id
                .keys()
                .any(|k| k != id && k.hex().starts_with(&hex[..n]))
        {
            n += 1;
        }
        hex[..n].to_string()
    }
}
