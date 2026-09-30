//! In-memory sync core (Repository-Semantics §4, Issue #21). No transport,
//! no wire encoding, no persistence.
//!
//! * §4.2 messages: [`Have`], [`Ops`], [`Need`], [`CheckpointsMsg`].
//! * §4.3 delta: [`delta`] computes exactly `Δ = V_B \ downclose(F_A ∩ V_B)`
//!   in `<` order. The VV hint is never used to exclude ops.
//! * §4.4/§4.5 receive: [`SyncReplica::receive_ops`] buffers ops with unknown
//!   deps (or a missing blob), accepts §2.2-invalid ops into the log (they
//!   become `invalid-op` conflicts in `tree::evaluate`), and rejects
//!   equivocation and Lamport violations with a local D-008
//!   [`RejectWarning`].
//! * Blobs: [`SyncReplica::answer`] (Issue #21) attaches the bytes of every
//!   blob a batch's `SetContent` ops name. [`pull`] (Issue #34) instead
//!   sends each batch without bytes ([`SyncReplica::answer_ops`]); the
//!   receiver names the blob ids it lacks ([`SyncReplica::lacking_blobs`],
//!   a `NeedBlobs` step) and only those bytes are sent, so a receiver never
//!   gets a blob it already holds. Either way the receiver verifies SHA-256
//!   against the `BlobId` and ignores duplicates. A `SetContent` is applied only when
//!   its deps *and* its blob are present; otherwise it stays pending, exactly
//!   like an op with an unknown dep. A blob whose bytes do not hash to its id
//!   is dropped with a [`RejectReason::CorruptBlob`] warning; the ops that
//!   need it stay pending until a correct copy arrives.
//! * Replica ids: [`new_replica_id`] draws 128 bits from the OS CSPRNG
//!   (`getrandom`). Duplicate ids (a copied `.hub/`) are detected when an op
//!   authored under our own id arrives that we never authored
//!   ([`RejectReason::DuplicateReplica`], accepted but flagged), and when two
//!   payloads share one OpId ([`RejectReason::Equivocation`]).
//!
//! State is computed with the spec fold `tree::evaluate` over the applied
//! set (§2.3); incremental application is an optimization out of scope here.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::checkpoint::{version_id, Checkpoint, VersionId};
use crate::projection::{project, Projection};
use crate::tree::{
    evaluate, BlobId, BlobStore, Conflict, NodeId, Op, OpId, Payload, ReplicaId, State,
};
use crate::version::{Store, Version, VersionVector};

/// Transport-level identity of the peer a message came from (§4.5). In this
/// in-memory core it is the sending replica's id.
pub type PeerId = ReplicaId;

/// §4.2 `Have`: the requester's frontier (authoritative) and an optional VV
/// (hint only; never used to exclude ops, §4.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Have {
    pub frontier: Vec<OpId>,
    pub vv_hint: Option<VersionVector>,
}

/// §4.2 `Ops`: a `<`-sorted batch. `blobs` is the Issue #21 extension: the
/// bytes of every blob referenced by a `SetContent` in `ops`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ops {
    pub ops: Vec<Op>,
    pub blobs: Vec<(BlobId, Vec<u8>)>,
    pub sender_frontier: Vec<OpId>,
    pub done: bool,
}

/// §4.2 `Need`: ids the requester lacks (deps of pending ops, or entries of
/// `sender_frontier` still missing).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Need {
    pub ids: Vec<OpId>,
}

/// §4.2 `Checkpoints`: published checkpoint records only (§4.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointsMsg {
    pub records: Vec<Checkpoint>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// Duplicate id, different op (§2.6 #1). Keep-first, local only.
    Equivocation,
    /// A dep `d` with `d ≮ o` (§2.6 #2).
    LamportViolation,
    /// Blob bytes do not hash to the claimed id (Issue #21).
    CorruptBlob,
    /// An op authored under our own replica id that we never authored:
    /// our id is in use elsewhere (copied `.hub/`). The op is accepted.
    DuplicateReplica,
}

/// D-008 warning (§4.5 fields). Local; never sent, never replicated.
/// `first_seen` is a local logical tick (receive counter), not wall time, so
/// tests stay deterministic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectWarning {
    pub peer: PeerId,
    pub author: Option<ReplicaId>,
    pub rejected: Option<OpId>,
    pub reason: RejectReason,
    pub detail: String,
    /// Pending ops with `rejected` in their deps* (or waiting for the corrupt
    /// blob); recomputed on every [`SyncReplica::warnings`] call.
    pub stuck: Vec<OpId>,
    pub first_seen: u64,
}

/// Draw a replica id from the OS CSPRNG. Never returns `ReplicaId::ZERO`.
pub fn new_replica_id() -> ReplicaId {
    loop {
        let mut b = [0u8; 16];
        getrandom::getrandom(&mut b).expect("OS random source unavailable");
        let r = u128::from_be_bytes(b);
        if r != 0 {
            return ReplicaId(r);
        }
    }
}

/// §4.3: `Δ = V_B \ downclose(F_A ∩ V_B)` in `<` order. `store` must be the
/// sender's applied (causally closed) set. `have.vv_hint` is ignored.
pub fn delta(store: &Store, have: &Have) -> Vec<Op> {
    let resolvable = have
        .frontier
        .iter()
        .copied()
        .filter(|f| store.contains_key(f));
    let known = Version::downclose(store, resolvable).expect("store is causally closed");
    store
        .values()
        .filter(|o| !known.contains(o.id))
        .cloned()
        .collect()
}

/// A replica holding an applied op set, a pending buffer, blobs, warnings
/// and a published checkpoint set.
#[derive(Clone, Debug)]
pub struct SyncReplica {
    id: ReplicaId,
    counter: u64,
    store: Store,
    pending: BTreeMap<OpId, (Op, PeerId)>,
    /// Issue #28 ready index over `pending`: waiters per missing dep and per
    /// missing blob, the count of unmet conditions per pending op, and the
    /// pending ops that are ready (so draining is not a scan per op).
    wait_dep: HashMap<OpId, Vec<OpId>>,
    wait_blob: HashMap<BlobId, Vec<OpId>>,
    unmet: HashMap<OpId, usize>,
    ready_q: BTreeSet<OpId>,
    rejected: BTreeMap<OpId, Op>,
    blobs: BlobStore,
    warnings: Vec<RejectWarning>,
    tick: u64,
    published: Vec<Checkpoint>,
    /// Issue #34 instrumentation: blob bytes (and blobs) received.
    blob_bytes_in: u64,
    blobs_in: usize,
}

impl SyncReplica {
    /// A replica with a fresh CSPRNG id.
    pub fn new() -> Self {
        Self::with_id(new_replica_id())
    }

    /// A replica with a given id (tests; loading an existing `.hub/`).
    pub fn with_id(id: ReplicaId) -> Self {
        assert_ne!(id, ReplicaId::ZERO, "ZERO is reserved");
        SyncReplica {
            id,
            counter: 0,
            store: Store::new(),
            pending: BTreeMap::new(),
            wait_dep: HashMap::new(),
            wait_blob: HashMap::new(),
            unmet: HashMap::new(),
            ready_q: BTreeSet::new(),
            rejected: BTreeMap::new(),
            blobs: BlobStore::new(),
            warnings: Vec::new(),
            tick: 0,
            published: Vec::new(),
            blob_bytes_in: 0,
            blobs_in: 0,
        }
    }

    /// Rebuild a replica from persisted parts (Issue #22: `.hub/` on disk).
    /// `store` must be causally closed and every blob its `SetContent` ops
    /// name must be in `blobs`. `pending` ops are re-buffered (and applied
    /// if they became ready); `warnings` keep their fields, `stuck` is
    /// recomputed on read. The rejected set is not persisted: a rejected op
    /// that arrives again is re-rejected, and the warning dedups.
    pub fn from_parts(
        id: ReplicaId,
        store: Store,
        blobs: BlobStore,
        pending: Vec<(Op, PeerId)>,
        warnings: Vec<RejectWarning>,
        published: Vec<Checkpoint>,
    ) -> Self {
        let mut r = SyncReplica::with_id(id);
        r.tick = warnings.iter().map(|w| w.first_seen).max().unwrap_or(0);
        r.counter = store.keys().map(|o| o.counter).max().unwrap_or(0);
        r.store = store;
        r.blobs = blobs;
        r.warnings = warnings;
        r.published = published;
        // A repeated id keeps the last entry, as the pre-#28 insert did.
        let pending: BTreeMap<OpId, (Op, PeerId)> =
            pending.into_iter().map(|(o, p)| (o.id, (o, p))).collect();
        for (id, (op, peer)) in pending {
            if !r.store.contains_key(&id) {
                r.enqueue(op, peer);
            }
        }
        r.drain();
        r
    }

    /// Buffered ops with the peer each came from, in `<` order.
    pub fn pending_ops(&self) -> Vec<(Op, PeerId)> {
        self.pending.values().cloned().collect()
    }

    pub fn id(&self) -> ReplicaId {
        self.id
    }
    pub fn store(&self) -> &Store {
        &self.store
    }
    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
    pub fn version(&self) -> Version {
        Version::from_ops(&self.store, self.store.keys().copied().collect())
            .expect("applied set is causally closed")
    }
    pub fn frontier(&self) -> Vec<OpId> {
        self.version().frontier(&self.store)
    }
    pub fn vv(&self) -> VersionVector {
        self.version().vv()
    }
    pub fn version_id(&self) -> VersionId {
        version_id(&self.store, &self.version())
    }
    pub fn state(&self) -> State {
        evaluate(self.store.values()).state
    }
    pub fn conflicts(&self) -> BTreeSet<Conflict> {
        evaluate(self.store.values()).conflicts
    }
    pub fn projection(&self) -> Projection {
        project(&self.state())
    }

    /// Author an op on the current frontier (§2.1 Lamport rule). No
    /// authoring-time validation: callers (and tests of buggy peers) may mint
    /// §2.2-invalid payloads.
    pub fn author(&mut self, payload: Payload) -> Op {
        let deps: BTreeSet<OpId> = self.frontier().into_iter().collect();
        self.author_with_deps(payload, deps)
    }

    /// Author with explicit deps (a subset of the applied set). Used to model
    /// peers that violate §2.6 #7 (own-chain), which §4.5 accepts.
    pub fn author_with_deps(&mut self, payload: Payload, deps: BTreeSet<OpId>) -> Op {
        assert!(deps.iter().all(|d| self.store.contains_key(d)));
        let maxdep = deps.iter().map(|d| d.counter).max().unwrap_or(0);
        let maxown = self.store.keys().map(|d| d.counter).max().unwrap_or(0);
        self.counter = 1 + self.counter.max(maxdep).max(maxown);
        let op = Op {
            id: NodeId {
                counter: self.counter,
                replica: self.id,
            },
            deps,
            payload,
        };
        if let Payload::SetContent { blob, .. } = &op.payload {
            assert!(self.blobs.contains(blob), "put the blob before SetContent");
        }
        self.store_op(op.clone());
        op
    }

    /// Store blob bytes locally (before authoring a SetContent).
    pub fn put_blob(&mut self, bytes: &[u8]) -> BlobId {
        let id = self.blobs.put(bytes);
        let w = self.wait_blob.remove(&id);
        self.satisfied(w);
        id
    }

    /// §4.2 `Have` for the current applied version.
    pub fn have(&self, with_hint: bool) -> Have {
        Have {
            frontier: self.frontier(),
            vv_hint: with_hint.then(|| self.vv()),
        }
    }

    /// Answer a `Have` with `Ops` batches of at most `batch` ops (>= 1).
    /// The last batch has `done = true`; an empty delta is one empty batch.
    pub fn answer(&self, have: &Have, batch: usize) -> Vec<Ops> {
        let d = delta(&self.store, have);
        let sender_frontier = self.frontier();
        let chunks: Vec<&[Op]> = if d.is_empty() {
            vec![&[]]
        } else {
            d.chunks(batch.max(1)).collect()
        };
        let n = chunks.len();
        chunks
            .into_iter()
            .enumerate()
            .map(|(i, c)| Ops {
                ops: c.to_vec(),
                blobs: self.blobs_for(c),
                sender_frontier: sender_frontier.clone(),
                done: i + 1 == n,
            })
            .collect()
    }

    /// [`SyncReplica::answer`] without blob bytes (Issue #34): the receiver
    /// asks for the blobs it lacks with [`SyncReplica::lacking_blobs`].
    pub fn answer_ops(&self, have: &Have, batch: usize) -> Vec<Ops> {
        let d = delta(&self.store, have);
        let sender_frontier = self.frontier();
        let n = d.len().div_ceil(batch.max(1)).max(1);
        let mut out: Vec<Ops> = d
            .chunks(batch.max(1))
            .map(|c| Ops {
                ops: c.to_vec(),
                blobs: Vec::new(),
                sender_frontier: sender_frontier.clone(),
                done: false,
            })
            .collect();
        if out.is_empty() {
            out.push(Ops {
                ops: Vec::new(),
                blobs: Vec::new(),
                sender_frontier,
                done: false,
            });
        }
        debug_assert_eq!(out.len(), n);
        out.last_mut().expect("one batch").done = true;
        out
    }

    /// Blob ids named by `SetContent` ops in `ops` that this replica does
    /// not hold, deduplicated, sorted (the Issue #34 `NeedBlobs` request).
    pub fn lacking_blobs(&self, ops: &[Op]) -> Vec<BlobId> {
        ops.iter()
            .filter_map(|o| match &o.payload {
                Payload::SetContent { blob, .. } if !self.blobs.contains(blob) => Some(*blob),
                _ => None,
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Blob bytes received so far (Issue #34 instrumentation).
    pub fn blob_bytes_received(&self) -> u64 {
        self.blob_bytes_in
    }
    /// Blobs received so far (Issue #34 instrumentation).
    pub fn blobs_received(&self) -> usize {
        self.blobs_in
    }

    /// Answer a `Need`: the named ops we hold (with their blobs), `<` order.
    /// Not causally closed by itself; the receiver buffers as needed.
    pub fn answer_need(&self, need: &Need) -> Ops {
        let ops: Vec<Op> = need
            .ids
            .iter()
            .filter_map(|i| self.store.get(i).map(|o| (o.id, o.clone())))
            .collect::<BTreeMap<_, _>>()
            .into_values()
            .collect();
        Ops {
            blobs: self.blobs_for(&ops),
            ops,
            sender_frontier: self.frontier(),
            done: true,
        }
    }

    /// Blob bytes for every SetContent in `ops`, deduplicated.
    pub fn blobs_for(&self, ops: &[Op]) -> Vec<(BlobId, Vec<u8>)> {
        let ids: BTreeSet<BlobId> = ops
            .iter()
            .filter_map(|o| match &o.payload {
                Payload::SetContent { blob, .. } => Some(*blob),
                _ => None,
            })
            .collect();
        ids.into_iter()
            .map(|b| {
                (
                    b,
                    self.blobs.get(&b).expect("sender holds its blobs").to_vec(),
                )
            })
            .collect()
    }

    /// `Need` for the missing deps of pending ops plus missing entries of
    /// `sender_frontier` (§4.4).
    pub fn need(&self, sender_frontier: &[OpId]) -> Need {
        let mut ids: BTreeSet<OpId> = BTreeSet::new();
        for (op, _) in self.pending.values() {
            for d in &op.deps {
                if !self.store.contains_key(d) && !self.pending.contains_key(d) {
                    ids.insert(*d);
                }
            }
        }
        for f in sender_frontier {
            if !self.store.contains_key(f) && !self.pending.contains_key(f) {
                ids.insert(*f);
            }
        }
        Need {
            ids: ids.into_iter().collect(),
        }
    }

    /// Receive one `Ops` batch from `peer`: blobs first (verified), then ops
    /// one at a time in arrival order.
    pub fn receive_ops(&mut self, peer: PeerId, msg: &Ops) {
        for (id, bytes) in &msg.blobs {
            self.receive_blob(peer, *id, bytes);
        }
        for op in &msg.ops {
            self.receive(peer, op.clone());
        }
    }

    /// Verify and store one blob. Returns false (and warns) if corrupt.
    pub fn receive_blob(&mut self, peer: PeerId, id: BlobId, bytes: &[u8]) -> bool {
        self.tick += 1;
        self.blob_bytes_in += bytes.len() as u64;
        self.blobs_in += 1;
        if BlobId::of(bytes) != id {
            self.warn(peer, None, None, RejectReason::CorruptBlob, id.hex());
            return false;
        }
        self.put_blob(bytes);
        self.drain();
        true
    }

    /// Receive one op (§4.5 table).
    pub fn receive(&mut self, peer: PeerId, op: Op) {
        self.tick += 1;
        let prior = self
            .store
            .get(&op.id)
            .or_else(|| self.pending.get(&op.id).map(|(o, _)| o))
            .or_else(|| self.rejected.get(&op.id));
        if let Some(prior) = prior {
            if *prior != op {
                let detail = format!("kept {:?}, rejected {:?}", prior.payload, op.payload);
                self.warn(
                    peer,
                    Some(op.id.replica),
                    Some(op.id),
                    RejectReason::Equivocation,
                    detail,
                );
            }
            return;
        }
        if op.id.replica == self.id {
            self.warn(
                peer,
                Some(op.id.replica),
                Some(op.id),
                RejectReason::DuplicateReplica,
                "op authored under our replica id that we never authored".into(),
            );
        }
        self.enqueue(op, peer);
        self.drain();
    }

    /// Buffer `op`, indexing what it still waits for: deps not in the store
    /// and, for a SetContent, a missing blob. Ready iff it waits for nothing.
    fn enqueue(&mut self, op: Op, peer: PeerId) {
        let id = op.id;
        let mut n = 0;
        for d in &op.deps {
            if !self.store.contains_key(d) {
                self.wait_dep.entry(*d).or_default().push(id);
                n += 1;
            }
        }
        if let Payload::SetContent { blob, .. } = &op.payload {
            if !self.blobs.contains(blob) {
                self.wait_blob.entry(*blob).or_default().push(id);
                n += 1;
            }
        }
        self.pending.insert(id, (op, peer));
        if n == 0 {
            self.ready_q.insert(id);
        } else {
            self.unmet.insert(id, n);
        }
    }

    /// One condition of each waiter became true.
    fn satisfied(&mut self, waiters: Option<Vec<OpId>>) {
        for w in waiters.unwrap_or_default() {
            let m = self.unmet.get_mut(&w).expect("waiter is pending");
            *m -= 1;
            if *m == 0 {
                self.unmet.remove(&w);
                self.ready_q.insert(w);
            }
        }
    }

    /// Insert into the applied set and wake ops waiting on it.
    fn store_op(&mut self, op: Op) {
        let id = op.id;
        self.store.insert(id, op);
        let w = self.wait_dep.remove(&id);
        self.satisfied(w);
    }

    /// Apply ready pending ops, smallest id first (the order the pre-#28
    /// scan of `pending` used), until none is ready.
    fn drain(&mut self) {
        while let Some(id) = self.ready_q.pop_first() {
            let (op, peer) = self.pending.remove(&id).unwrap();
            if let Some(d) = op.deps.iter().find(|d| **d >= op.id) {
                let detail = format!("dep {d:?} is not < {:?}", op.id);
                self.warn(
                    peer,
                    Some(op.id.replica),
                    Some(op.id),
                    RejectReason::LamportViolation,
                    detail,
                );
                self.rejected.insert(op.id, op);
                continue;
            }
            self.store_op(op);
        }
    }

    fn warn(
        &mut self,
        peer: PeerId,
        author: Option<ReplicaId>,
        rejected: Option<OpId>,
        reason: RejectReason,
        detail: String,
    ) {
        let dup = self.warnings.iter().any(|w| {
            w.peer == peer && w.rejected == rejected && w.reason == reason && w.detail == detail
        });
        if !dup {
            self.warnings.push(RejectWarning {
                peer,
                author,
                rejected,
                reason,
                detail,
                stuck: Vec::new(),
                first_seen: self.tick,
            });
        }
    }

    /// D-008 warnings with `stuck` recomputed from the current buffer.
    pub fn warnings(&self) -> Vec<RejectWarning> {
        self.warnings
            .iter()
            .map(|w| {
                let mut w = w.clone();
                w.stuck = match (&w.reason, w.rejected) {
                    (RejectReason::LamportViolation, Some(r)) => self.stuck_below(r),
                    (RejectReason::CorruptBlob, _) => self
                        .pending
                        .values()
                        .filter(|(o, _)| {
                            matches!(&o.payload, Payload::SetContent { blob, .. }
                                if blob.hex() == w.detail)
                        })
                        .map(|(o, _)| o.id)
                        .collect(),
                    _ => Vec::new(),
                };
                w
            })
            .collect()
    }

    /// Pending ops with `r` in their deps* (through pending ops only).
    fn stuck_below(&self, r: OpId) -> Vec<OpId> {
        let mut stuck: BTreeSet<OpId> = BTreeSet::new();
        // Pending ops sorted by `<`; a well-formed dep is smaller, but a
        // chain through bad ops may not be, so iterate to a fixpoint.
        loop {
            let before = stuck.len();
            for (id, (o, _)) in &self.pending {
                if o.deps.iter().any(|d| *d == r || stuck.contains(d)) {
                    stuck.insert(*id);
                }
            }
            if stuck.len() == before {
                return stuck.into_iter().collect();
            }
        }
    }

    // ---------- published checkpoints (§4.6, minimal for §4.7 #10) ----------

    /// Add a record to the published set (idempotent).
    pub fn publish(&mut self, c: Checkpoint) {
        if !self.published.contains(&c) {
            self.published.push(c);
        }
    }
    pub fn published(&self) -> &[Checkpoint] {
        &self.published
    }
    /// A sender sends only records whose frontier it can resolve (§4.6).
    pub fn checkpoints_msg(&self) -> CheckpointsMsg {
        CheckpointsMsg {
            records: self
                .published
                .iter()
                .filter(|c| c.frontier().iter().all(|f| self.store.contains_key(f)))
                .cloned()
                .collect(),
        }
    }
    pub fn receive_checkpoints(&mut self, msg: &CheckpointsMsg) {
        for c in &msg.records {
            self.publish(c.clone());
        }
    }
    /// Pending iff some frontier id is not in the applied set (§3.4).
    pub fn checkpoint_pending(&self, c: &Checkpoint) -> bool {
        !c.frontier().iter().all(|f| self.store.contains_key(f))
    }
}

impl Default for SyncReplica {
    fn default() -> Self {
        Self::new()
    }
}

/// One full pull `a ← b`: `Have`, all batches, then one `Need` round.
/// Blob bytes come from `b`'s in-memory store; only blobs `a` lacks are
/// sent (Issue #34).
pub fn pull(a: &mut SyncReplica, b: &SyncReplica, batch: usize, with_hint: bool) {
    let r: Result<(), std::convert::Infallible> =
        pull_with(a, b, batch, with_hint, &mut |b, id| {
            Ok(b.blobs.get(id).expect("sender holds its blobs").to_vec())
        });
    r.unwrap_or_else(|e| match e {});
}

/// Sender-side blob source: the bytes of blob `id`, which the sender holds.
pub type FetchBlob<'a, E> = dyn FnMut(&SyncReplica, &BlobId) -> Result<Vec<u8>, E> + 'a;

/// [`pull`] with the sender's blob bytes supplied by `fetch` (Issue #34: a
/// persisted sender reads only the blobs it sends). Every batch is sent
/// without bytes, the receiver names the blobs it lacks, and exactly those
/// are fetched and received ahead of the batch's ops, as in
/// [`SyncReplica::receive_ops`].
pub fn pull_with<E>(
    a: &mut SyncReplica,
    b: &SyncReplica,
    batch: usize,
    with_hint: bool,
    fetch: &mut FetchBlob<'_, E>,
) -> Result<(), E> {
    let mut fill = |a: &SyncReplica, mut msg: Ops| -> Result<Ops, E> {
        for id in a.lacking_blobs(&msg.ops) {
            msg.blobs.push((id, fetch(b, &id)?));
        }
        Ok(msg)
    };
    let have = a.have(with_hint);
    let mut last_frontier = Vec::new();
    for msg in b.answer_ops(&have, batch) {
        let msg = fill(a, msg)?;
        a.receive_ops(b.id(), &msg);
        last_frontier = msg.sender_frontier.clone();
    }
    let need = a.need(&last_frontier);
    if !need.ids.is_empty() {
        let mut msg = b.answer_need(&need);
        msg.blobs.clear();
        let msg = fill(a, msg)?;
        a.receive_ops(b.id(), &msg);
    }
    Ok(())
}

/// Full sync: pull both ways, then exchange published checkpoints.
pub fn sync(a: &mut SyncReplica, b: &mut SyncReplica, batch: usize) {
    pull(a, b, batch, true);
    pull(b, a, batch, true);
    exchange_checkpoints(a, b);
}

/// [`sync`] with blob sources for each side (see [`pull_with`]):
/// `fetch_a` reads `a`'s blobs (sent to `b`), `fetch_b` reads `b`'s.
pub fn sync_with<E>(
    a: &mut SyncReplica,
    b: &mut SyncReplica,
    batch: usize,
    fetch_a: &mut FetchBlob<'_, E>,
    fetch_b: &mut FetchBlob<'_, E>,
) -> Result<(), E> {
    pull_with(a, b, batch, true, fetch_b)?;
    pull_with(b, a, batch, true, fetch_a)?;
    exchange_checkpoints(a, b);
    Ok(())
}

fn exchange_checkpoints(a: &mut SyncReplica, b: &mut SyncReplica) {
    let ma = a.checkpoints_msg();
    let mb = b.checkpoints_msg();
    b.receive_checkpoints(&ma);
    a.receive_checkpoints(&mb);
}
