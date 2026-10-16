//! In-memory replicated tree (Repository-Semantics §1-§2, structural ops only).
//!
//! The spec is `State(Ops) = fold(apply, S0, sort(Ops by <))` (§2.3). [`evaluate`]
//! implements that fold and derives the conflict set; [`Replica`] implements the
//! incremental undo-do-redo optimization with causal buffering and duplicate
//! suppression, and must agree with [`evaluate`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use sha2::{Digest, Sha256};

/// Content address of a whole-file blob: SHA-256 of its bytes (Issue #18).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlobId(pub [u8; 32]);

impl BlobId {
    /// SHA-256 of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        BlobId(Sha256::digest(bytes).into())
    }
    /// Content of a newly created File: the empty blob.
    pub fn empty() -> Self {
        Self::of(&[])
    }
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl std::fmt::Debug for BlobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BlobId({})", &self.hex()[..12])
    }
}

/// In-memory content-addressed blob store (v0; no disk, no chunking).
#[derive(Clone, Debug, Default)]
pub struct BlobStore {
    blobs: BTreeMap<BlobId, Vec<u8>>,
    /// Issue #34: ids known to be held (for example on disk) whose bytes
    /// are not loaded. [`BlobStore::contains`] is true for them,
    /// [`BlobStore::get`] is `None`.
    held: BTreeSet<BlobId>,
}

impl BlobStore {
    pub fn new() -> Self {
        let mut s = BlobStore::default();
        s.put(&[]);
        s
    }
    /// Store `bytes`, returning their id. Idempotent.
    pub fn put(&mut self, bytes: &[u8]) -> BlobId {
        let id = BlobId::of(bytes);
        self.blobs.entry(id).or_insert_with(|| bytes.to_vec());
        id
    }
    pub fn get(&self, id: &BlobId) -> Option<&[u8]> {
        self.blobs.get(id).map(|v| v.as_slice())
    }
    pub fn contains(&self, id: &BlobId) -> bool {
        self.blobs.contains_key(id) || self.held.contains(id)
    }
    /// Record that `id` is held elsewhere without loading its bytes
    /// (Issue #34: a sync never reads blobs it does not send).
    pub fn mark_held(&mut self, id: BlobId) {
        if !self.blobs.contains_key(&id) {
            self.held.insert(id);
        }
    }
    /// Ids whose bytes are loaded (excludes [`BlobStore::mark_held`] ids).
    pub fn loaded(&self) -> impl Iterator<Item = (&BlobId, &[u8])> {
        self.blobs.iter().map(|(k, v)| (k, v.as_slice()))
    }
    pub fn len(&self) -> usize {
        self.blobs.len()
    }
    pub fn is_empty(&self) -> bool {
        self.blobs.is_empty()
    }
}

/// 128-bit replica id. `ZERO` is reserved and never generated (§1.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReplicaId(pub u128);

impl ReplicaId {
    pub const ZERO: ReplicaId = ReplicaId(0);
}

/// `(Counter, ReplicaId)`, ordered lexicographically (§1.1, §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId {
    pub counter: u64,
    pub replica: ReplicaId,
}

/// A create's NodeId equals its OpId (§1.1).
pub type OpId = NodeId;

pub const ROOT: NodeId = NodeId {
    counter: 0,
    replica: ReplicaId::ZERO,
};
pub const TRASH: NodeId = NodeId {
    counter: 1,
    replica: ReplicaId::ZERO,
};

impl NodeId {
    pub fn new(counter: u64, replica: u128) -> Self {
        NodeId {
            counter,
            replica: ReplicaId(replica),
        }
    }
    pub fn is_reserved(self) -> bool {
        self.replica == ReplicaId::ZERO
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Root,
    Directory,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub id: NodeId,
    pub kind: Kind,
    pub parent: Option<NodeId>,
    pub name: String,
    /// Whole-file content of a File (`Some`, empty blob at creation);
    /// `None` for Root/Directory (Issue #18).
    pub content: Option<BlobId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Payload {
    Create {
        kind: Kind,
        parent: NodeId,
        name: String,
    },
    Move {
        node: NodeId,
        new_parent: NodeId,
        new_name: String,
    },
    Delete {
        node: NodeId,
    },
    /// Whole-file replace of a File's content (§2.2 `SetContent`, v0 Bytes).
    SetContent {
        node: NodeId,
        blob: BlobId,
    },
    /// Edit of one text epoch of a File (Text-CRDT 7.4.1, Issue #53).
    /// Core only in T1: no log record, wire or snapshot encoding yet.
    TextEdit(crate::text::TextEdit),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Op {
    pub id: OpId,
    pub deps: BTreeSet<OpId>,
    pub payload: Payload,
}

impl Op {
    /// Node targeted by a Move/Delete.
    pub fn moved_node(&self) -> Option<NodeId> {
        match &self.payload {
            Payload::Move { node, .. } | Payload::Delete { node } => Some(*node),
            Payload::Create { .. } | Payload::SetContent { .. } | Payload::TextEdit(_) => None,
        }
    }

    /// Node targeted by a SetContent.
    pub fn content_node(&self) -> Option<NodeId> {
        match &self.payload {
            Payload::SetContent { node, .. } => Some(*node),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Conflict {
    /// Statically invalid op (§2.2); a no-op.
    InvalidOp { op: OpId },
    /// Skipped cycle-forming move (§2.4).
    Cycle {
        winner: OpId,
        loser: OpId,
        node: NodeId,
    },
    /// Move overwritten by a concurrent move of the same node (§2.4).
    SameNodeMove {
        winner: OpId,
        loser: OpId,
        node: NodeId,
    },
    /// Informational (§2.5 Option A): `node` stays live although a concurrent
    /// delete trashed one of its ancestors.
    Revived {
        node: NodeId,
        delete: OpId,
        mv: OpId,
    },
    /// Content set by `loser` overwritten by a concurrent SetContent `winner`
    /// of the same node (§2.3, Issue #18). Both blobs are kept.
    Content {
        node: NodeId,
        winner: OpId,
        loser: OpId,
        winner_blob: BlobId,
        loser_blob: BlobId,
    },
    /// Informational (D-014, Issue #20): `edit` set content of `node` while
    /// `delete` (not causally after `edit`) had trashed `node` or an ancestor;
    /// the edit revived the deleted node to its pre-delete parent. `delete`
    /// is not a loser; the conflict keeps its intent visible.
    EditRevived {
        node: NodeId,
        edit: OpId,
        delete: OpId,
    },
}

/// Tree state: every node record ever validly created, keyed by id.
pub type State = BTreeMap<NodeId, Node>;

pub fn initial_state() -> State {
    let mut s = State::new();
    s.insert(
        ROOT,
        Node {
            id: ROOT,
            kind: Kind::Root,
            parent: None,
            name: String::new(),
            content: None,
        },
    );
    s.insert(
        TRASH,
        Node {
            id: TRASH,
            kind: Kind::Directory,
            parent: None,
            name: String::new(),
            content: None,
        },
    );
    s
}

/// Longest valid name in bytes (Remote-Sync §5.2.5, Issue #43): one op
/// record always fits in one wire frame. Longer names are §2.2 invalid.
pub const MAX_NAME_BYTES: usize = 4096;

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && name.len() <= MAX_NAME_BYTES
}

/// True if `a` is `b` or an ancestor of `b`.
pub fn is_ancestor_or_self(state: &State, a: NodeId, b: NodeId) -> bool {
    let mut cur = Some(b);
    let mut steps = 0;
    while let Some(c) = cur {
        if c == a {
            return true;
        }
        steps += 1;
        if steps > state.len() {
            return false;
        }
        cur = state.get(&c).and_then(|n| n.parent);
    }
    false
}

pub fn is_live(state: &State, n: NodeId) -> bool {
    state.contains_key(&n) && is_ancestor_or_self(state, ROOT, n)
}

/// Undo record for one applied op (used by [`Replica`]).
#[derive(Clone, Debug)]
enum Undo {
    None,
    RemoveNode(NodeId),
    Restore {
        node: NodeId,
        parent: Option<NodeId>,
        name: String,
        setter: OpId,
    },
    Content {
        node: NodeId,
        prev: Option<BlobId>,
    },
}

/// Result of applying one op to a state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    Invalid,
    /// Cycle-forming move skipped; `winner` = greatest op that set a parent
    /// edge on the existing path from the target up to the moved node.
    Skipped {
        winner: OpId,
    },
}

/// Working state: node records plus, per node, the op that last set its parent.
#[derive(Clone, Debug)]
struct Work {
    state: State,
    setter: BTreeMap<NodeId, OpId>,
}

impl Work {
    fn new() -> Self {
        Work {
            state: initial_state(),
            setter: BTreeMap::new(),
        }
    }

    fn parent_ok(&self, p: NodeId) -> bool {
        matches!(
            self.state.get(&p).map(|n| n.kind),
            Some(Kind::Root | Kind::Directory)
        )
    }

    fn apply(&mut self, op: &Op) -> (Outcome, Undo) {
        match &op.payload {
            Payload::Create { kind, parent, name } => {
                if op.id.is_reserved()
                    || *kind == Kind::Root
                    || !valid_name(name)
                    || !self.parent_ok(*parent)
                    || self.state.contains_key(&op.id)
                {
                    return (Outcome::Invalid, Undo::None);
                }
                self.state.insert(
                    op.id,
                    Node {
                        id: op.id,
                        kind: *kind,
                        parent: Some(*parent),
                        name: name.clone(),
                        content: (*kind == Kind::File).then(BlobId::empty),
                    },
                );
                self.setter.insert(op.id, op.id);
                (Outcome::Applied, Undo::RemoveNode(op.id))
            }
            Payload::SetContent { node, blob } => match self.state.get_mut(node) {
                Some(rec) if rec.kind == Kind::File && !node.is_reserved() => {
                    let prev = rec.content.replace(*blob);
                    (Outcome::Applied, Undo::Content { node: *node, prev })
                }
                _ => (Outcome::Invalid, Undo::None),
            },
            // Structural part only; the text pass (`text::text_pass`)
            // decides 7.4.1/7.4.2 validity and never changes the tree.
            Payload::TextEdit(t) => match self.state.get(&t.node) {
                Some(rec) if rec.kind == Kind::File && !t.node.is_reserved() => {
                    (Outcome::Applied, Undo::None)
                }
                _ => (Outcome::Invalid, Undo::None),
            },
            Payload::Move { .. } | Payload::Delete { .. } => {
                let (node, new_parent, new_name) = match &op.payload {
                    Payload::Move {
                        node,
                        new_parent,
                        new_name,
                    } => (*node, *new_parent, new_name.clone()),
                    Payload::Delete { node } => match self.state.get(node) {
                        Some(n) => (*node, TRASH, n.name.clone()),
                        None => return (Outcome::Invalid, Undo::None),
                    },
                    Payload::Create { .. } | Payload::SetContent { .. } | Payload::TextEdit(_) => {
                        unreachable!()
                    }
                };
                if node.is_reserved()
                    || !self.state.contains_key(&node)
                    || !self.parent_ok(new_parent)
                    || !valid_name(&new_name)
                {
                    return (Outcome::Invalid, Undo::None);
                }
                if is_ancestor_or_self(&self.state, node, new_parent) {
                    // Winner: greatest parent-setting op on path new_parent -> node.
                    let mut winner = None::<OpId>;
                    let mut cur = new_parent;
                    while cur != node {
                        let s = self.setter[&cur];
                        winner = Some(winner.map_or(s, |w| w.max(s)));
                        cur = self.state[&cur].parent.expect("path reaches node");
                    }
                    // n == p (self-move): no edge; name the op itself.
                    return (
                        Outcome::Skipped {
                            winner: winner.unwrap_or(op.id),
                        },
                        Undo::None,
                    );
                }
                let rec = self.state.get_mut(&node).unwrap();
                let undo = Undo::Restore {
                    node,
                    parent: rec.parent,
                    name: std::mem::replace(&mut rec.name, new_name),
                    setter: self.setter[&node],
                };
                rec.parent = Some(new_parent);
                self.setter.insert(node, op.id);
                (Outcome::Applied, undo)
            }
        }
    }

    fn undo(&mut self, u: Undo) {
        match u {
            Undo::None => {}
            Undo::RemoveNode(n) => {
                self.state.remove(&n);
                self.setter.remove(&n);
            }
            Undo::Restore {
                node,
                parent,
                name,
                setter,
            } => {
                let rec = self.state.get_mut(&node).unwrap();
                rec.parent = parent;
                rec.name = name;
                self.setter.insert(node, setter);
            }
            Undo::Content { node, prev } => {
                self.state.get_mut(&node).unwrap().content = prev;
            }
        }
    }
}

/// Full evaluation of an op set: final state, conflicts, per-op outcome.
#[derive(Clone, Debug)]
pub struct Evaluation {
    pub state: State,
    pub conflicts: BTreeSet<Conflict>,
    pub outcomes: BTreeMap<OpId, Outcome>,
    /// Text content, notices and derived blobs (Issue #53); empty when
    /// the op set has no `TextEdit`.
    pub text: crate::text::TextEval,
}

/// `deps*` closure: for each op, the set of all its causal predecessors.
/// Deps not in `ops` are ignored.
pub fn causal_past(ops: &BTreeMap<OpId, Op>) -> BTreeMap<OpId, BTreeSet<OpId>> {
    let mut past: BTreeMap<OpId, BTreeSet<OpId>> = BTreeMap::new();
    // Lamport consistency: deps < op, so ascending order visits deps first.
    for (id, op) in ops {
        let mut p = BTreeSet::new();
        for d in &op.deps {
            if let Some(dp) = past.get(d) {
                p.insert(*d);
                p.extend(dp.iter().copied());
            }
        }
        past.insert(*id, p);
    }
    past
}

/// Exact causality over an op set without materializing `deps*` (Issue #28).
///
/// Same relation as [`causal_past`]: an edge `d -> o` counts iff `d` is in
/// the set and `d < o` (other deps are ignored), and `a ≺ b` is its
/// transitive closure. Each op carries a version vector `vv(o)[r]` = 1 + the
/// greatest counter of a replica-`r` op in `{o} ∪ deps*(o)` (0 = none),
/// computed as a pointwise max over its deps, so it is exact for any op set.
///
/// `vv(b)[a.replica] > a.counter` is necessary for `a ≺ b`. It is also
/// sufficient when replica `a.replica`'s ops form one causal chain in counter
/// order (§2.6 #7, own-chain), which is checked per replica while building.
/// For a replica that violates §2.6 #7 (the VV is only a hint, Issue #12),
/// [`Causality::precedes`] falls back to a pruned walk of the deps DAG.
/// Memory is O(ops × replicas); a query is O(1) under §2.6 #7.
#[derive(Clone, Debug)]
pub struct Causality {
    /// Op ids in `<` order; an op's index is its position (Issue #33: a
    /// sorted vector instead of a hash map, deps in CSR form).
    ids: Vec<OpId>,
    /// Kept edges (`d` in the set, `d < o`) as indices, per op in `<` order:
    /// `dep_list[dep_off[i]..dep_off[i + 1]]`.
    dep_off: Vec<u32>,
    dep_list: Vec<u32>,
    slot: HashMap<ReplicaId, usize>,
    /// Slot of each op's replica.
    op_slot: Vec<u32>,
    width: usize,
    /// Row-major `ops × width` version vectors (counter + 1, 0 = none).
    vv: Vec<u64>,
    /// Per replica slot: §2.6 #7 holds within the set.
    chain: Vec<bool>,
}

impl Causality {
    /// Build over `ops` (keys are the op ids, ascending).
    pub fn new(ops: &BTreeMap<OpId, Op>) -> Self {
        Self::of_sorted(ops.values())
    }

    /// [`Causality::new`] over ops given in ascending id order (Issue #54:
    /// the replica's log, without building a map).
    pub fn of_sorted<'a, I>(ops: I) -> Self
    where
        I: Iterator<Item = &'a Op> + Clone,
    {
        let ids: Vec<OpId> = ops.clone().map(|o| o.id).collect();
        let mut slot: HashMap<ReplicaId, usize> = HashMap::new();
        let mut op_slot = Vec::with_capacity(ids.len());
        let mut cache: Option<(ReplicaId, usize)> = None;
        for id in &ids {
            let s = match cache {
                Some((r, s)) if r == id.replica => s,
                _ => {
                    let n = slot.len();
                    let s = *slot.entry(id.replica).or_insert(n);
                    cache = Some((id.replica, s));
                    s
                }
            };
            op_slot.push(s as u32);
        }
        let width = slot.len().max(1);
        let mut vv = vec![0u64; ids.len() * width];
        let mut chain = vec![true; width];
        let mut last: Vec<u64> = vec![0; width];
        let mut dep_off = Vec::with_capacity(ids.len() + 1);
        let mut dep_list: Vec<u32> = Vec::with_capacity(ids.len());
        dep_off.push(0u32);
        let mut ds: Vec<usize> = Vec::new();
        for (i, op) in ops.enumerate() {
            ds.clear();
            for d in &op.deps {
                // Fast path: the previous op; else a search below `i`.
                let j = if i > 0 && ids[i - 1] == *d {
                    Some(i - 1)
                } else {
                    ids[..i].binary_search(d).ok()
                };
                if let Some(j) = j {
                    ds.push(j);
                }
            }
            let (done, rest) = vv.split_at_mut(i * width);
            let row = &mut rest[..width];
            for &j in &ds {
                for (x, &y) in row.iter_mut().zip(&done[j * width..(j + 1) * width]) {
                    *x = (*x).max(y);
                }
            }
            let s = op_slot[i] as usize;
            // The previous op of this replica (greatest smaller counter) must
            // be in deps*(op). While the replica's chain holds, that is
            // exactly `vv over deps >= prev + 1`.
            if last[s] != 0 && row[s] < last[s] {
                chain[s] = false;
            }
            last[s] = op.id.counter + 1;
            row[s] = op.id.counter + 1;
            dep_list.extend(ds.iter().map(|&j| j as u32));
            dep_off.push(dep_list.len() as u32);
        }
        Causality {
            ids,
            dep_off,
            dep_list,
            slot,
            op_slot,
            width,
            vv,
            chain,
        }
    }

    fn row(&self, i: usize) -> &[u64] {
        &self.vv[i * self.width..(i + 1) * self.width]
    }

    fn deps_of(&self, i: usize) -> &[u32] {
        &self.dep_list[self.dep_off[i] as usize..self.dep_off[i + 1] as usize]
    }

    fn idx(&self, id: OpId) -> Option<usize> {
        self.ids.binary_search(&id).ok()
    }

    /// `a ∈ deps*(b)` (strict), i.e. `causal_past(ops)[b].contains(a)`.
    /// False if either id is not in the set.
    pub fn precedes(&self, a: OpId, b: OpId) -> bool {
        let (Some(ia), Some(ib)) = (self.idx(a), self.idx(b)) else {
            return false;
        };
        self.precedes_at(ia, ib)
    }

    /// [`Causality::precedes`] by index (the op's position in `<` order).
    pub fn precedes_at(&self, ia: usize, ib: usize) -> bool {
        if ia >= ib {
            return false;
        }
        let a = self.ids[ia];
        let s = self.op_slot[ia] as usize;
        let need = a.counter + 1;
        if self.row(ib)[s] < need {
            return false;
        }
        if self.chain[s] {
            return true;
        }
        // §2.6 #7 violated for a's replica: exact walk from b, pruning ops
        // below a (edges only go down) and ops whose VV excludes a.
        let mut stack = vec![ib];
        let mut seen = HashSet::new();
        while let Some(x) = stack.pop() {
            for &d in self.deps_of(x) {
                let d = d as usize;
                if d == ia {
                    return true;
                }
                if d > ia && self.row(d)[s] >= need && seen.insert(d) {
                    stack.push(d);
                }
            }
        }
        false
    }

    /// Neither op precedes the other.
    pub fn concurrent(&self, a: OpId, b: OpId) -> bool {
        !self.precedes(a, b) && !self.precedes(b, a)
    }

    /// For ascending `xs`, whether each `x` precedes some later `y` in `xs`
    /// (is overwritten causally). O(|xs| × replicas) under §2.6 #7.
    fn overwritten(&self, xs: &[OpId]) -> Vec<bool> {
        let mut out = vec![false; xs.len()];
        let mut acc = vec![0u64; self.width];
        for k in (0..xs.len()).rev() {
            let x = xs[k];
            if let Some(&s) = self.slot.get(&x.replica) {
                let need = x.counter + 1;
                out[k] = acc[s] >= need
                    && (self.chain[s] || xs[k + 1..].iter().any(|&y| self.precedes(x, y)));
            }
            if let Some(i) = self.idx(x) {
                for (a, &v) in acc.iter_mut().zip(self.row(i)) {
                    *a = (*a).max(v);
                }
            }
        }
        out
    }

    /// [`Causality::overwritten`] for ops given by index (ascending).
    fn overwritten_at(&self, xs: &[usize]) -> Vec<bool> {
        let mut out = vec![false; xs.len()];
        let mut acc = vec![0u64; self.width];
        for k in (0..xs.len()).rev() {
            let i = xs[k];
            let s = self.op_slot[i] as usize;
            let need = self.ids[i].counter + 1;
            out[k] = acc[s] >= need
                && (self.chain[s] || xs[k + 1..].iter().any(|&j| self.precedes_at(i, j)));
            for (a, &v) in acc.iter_mut().zip(self.row(i)) {
                *a = (*a).max(v);
            }
        }
        out
    }
}

/// The specification: sorted fold (§2.3) plus derived conflicts (§2.4, §2.5).
///
/// Issue #28: same result as the pre-#28 form (kept as the `oracle` test
/// module) without its O(n²) parts: causality via [`Causality`] instead of
/// materialized predecessor sets, and no per-Move/Delete state snapshots
/// (the pre-delete parent is read during the fold; the `Revived` ancestry
/// check runs in a second fold only for concurrent delete/move pairs).
pub fn evaluate<'a>(ops: impl IntoIterator<Item = &'a Op>) -> Evaluation {
    evaluate_with(ops, &crate::text::NoBlobs)
}

/// [`evaluate`] with epoch blob bytes for text ops (Text-CRDT 7.4.2): a
/// `TextEdit` whose epoch blob is unavailable is accepted with the
/// deferred checks and has no head bytes.
pub fn evaluate_with<'a>(
    ops: impl IntoIterator<Item = &'a Op>,
    blobs: &dyn crate::text::BlobSource,
) -> Evaluation {
    let ops: BTreeMap<OpId, Op> = ops.into_iter().map(|o| (o.id, o.clone())).collect();
    evaluate_map(ops, blobs, None)
}

/// [`evaluate_with`], integrating text ops in `order` (a causal order of
/// the op ids) instead of `<`: same result (Text-CRDT 7.12 #1, tests).
pub fn evaluate_in_order(
    ops: &[Op],
    blobs: &dyn crate::text::BlobSource,
    order: &[OpId],
) -> Evaluation {
    let ops: BTreeMap<OpId, Op> = ops.iter().map(|o| (o.id, o.clone())).collect();
    evaluate_map(ops, blobs, Some(order))
}

fn evaluate_map(
    ops: BTreeMap<OpId, Op>,
    blobs: &dyn crate::text::BlobSource,
    order: Option<&[OpId]>,
) -> Evaluation {
    let causal = Causality::new(&ops);

    let mut w = Work::new();
    let mut outcomes = BTreeMap::new();
    let mut conflicts = BTreeSet::new();
    // Parent of the node just before each Delete (read before applying it).
    let mut parent_before: BTreeMap<OpId, Option<NodeId>> = BTreeMap::new();
    for (id, op) in &ops {
        if let Payload::Delete { node } = op.payload {
            parent_before.insert(*id, w.state.get(&node).and_then(|n| n.parent));
        }
        let (out, _) = w.apply(op);
        match &out {
            Outcome::Invalid => {
                conflicts.insert(Conflict::InvalidOp { op: *id });
            }
            Outcome::Skipped { winner } => {
                conflicts.insert(Conflict::Cycle {
                    winner: *winner,
                    loser: *id,
                    node: op.moved_node().unwrap(),
                });
            }
            Outcome::Applied => {}
        }
        outcomes.insert(*id, out);
    }
    // Text ops (Issue #53): 7.4.1/7.4.2 validity, then per-epoch docs.
    let mut text = crate::text::TextEval::default();
    let has_text = ops
        .values()
        .any(|o| matches!(o.payload, Payload::TextEdit(_)));
    if has_text {
        let (tv, invalid) = crate::text::text_pass(&ops, &outcomes, &causal, blobs, order);
        for id in invalid {
            outcomes.insert(id, Outcome::Invalid);
            conflicts.insert(Conflict::InvalidOp { op: id });
        }
        text = tv;
    }
    let text_nodes: BTreeSet<NodeId> = text
        .valid
        .iter()
        .map(|v| match &ops[v].payload {
            Payload::TextEdit(t) => t.node,
            _ => unreachable!(),
        })
        .collect();

    // Applied (non-skipped, valid) Move/Delete ops per node, ascending.
    let mut applied: BTreeMap<NodeId, Vec<OpId>> = BTreeMap::new();
    for (id, op) in &ops {
        if let (Some(n), Outcome::Applied) = (op.moved_node(), &outcomes[id]) {
            applied.entry(n).or_default().push(*id);
        }
    }
    // Same-node-move losers (§2.4, 3+ moves rule).
    for (n, ms) in &applied {
        let winner = *ms.last().unwrap();
        let over = causal.overwritten(ms);
        for (i, &m) in ms[..ms.len() - 1].iter().enumerate() {
            if !over[i] {
                conflicts.insert(Conflict::SameNodeMove {
                    winner,
                    loser: m,
                    node: *n,
                });
            }
        }
    }
    // Content (§2.3, Issue #18): winner = greatest Applied SetContent of a
    // node; every other one without a causally later SetContent of the same
    // node is a loser with exactly one `content` conflict.
    let mut sets: BTreeMap<NodeId, Vec<(OpId, BlobId)>> = BTreeMap::new();
    for (id, op) in &ops {
        if let (Payload::SetContent { node, blob }, Outcome::Applied) = (&op.payload, &outcomes[id])
        {
            sets.entry(*node).or_default().push((*id, *blob));
        }
    }
    for (n, es) in &sets {
        if text_nodes.contains(n) {
            continue;
        }
        let (winner, winner_blob) = *es.last().unwrap();
        let ids: Vec<OpId> = es.iter().map(|&(e, _)| e).collect();
        let over = causal.overwritten(&ids);
        for (i, &(e, loser_blob)) in es[..es.len() - 1].iter().enumerate() {
            if !over[i] {
                conflicts.insert(Conflict::Content {
                    node: *n,
                    winner,
                    loser: e,
                    winner_blob,
                    loser_blob,
                });
            }
        }
    }
    // Text-CRDT 7.4.3: content and conflicts of nodes with text ops.
    if !text_nodes.is_empty() {
        conflicts.extend(crate::text::derive(
            &ops, &outcomes, &causal, blobs, &mut text,
        ));
        for (n, nt) in &text.content {
            if let Some(rec) = w.state.get_mut(n) {
                rec.content = Some(nt.blob);
            }
        }
    }
    // Edit revives (D-014, Issue #20): post-fold, order-independent. A
    // valid TextEdit is an edit (Text-CRDT 7.4.4).
    let mut edits: Vec<(OpId, NodeId)> = sets
        .iter()
        .flat_map(|(n, es)| es.iter().map(move |&(e, _)| (e, *n)))
        .collect();
    edits.extend(text.valid.iter().map(|v| match &ops[v].payload {
        Payload::TextEdit(t) => (*v, t.node),
        _ => unreachable!(),
    }));
    edits.sort();
    let mut pre = BTreeMap::new();
    for (id, parent) in &parent_before {
        if let (Payload::Delete { node }, Outcome::Applied) = (&ops[id].payload, &outcomes[id]) {
            let ms = &applied[node];
            let prev = ms
                .iter()
                .position(|m| m == id)
                .and_then(|i| i.checked_sub(1));
            let prev = prev.map(|i| ms[i]);
            let parent = parent.expect("non-reserved node has parent");
            pre.insert(*id, pre_delete_parent(parent, prev, &pre));
        }
    }
    let revived = revive_by_edits(
        &mut w.state,
        &ops,
        &w.setter,
        &pre,
        |e, d| causal.precedes(e, d),
        &edits,
    );
    for (x, e, d) in revived {
        conflicts.insert(Conflict::EditRevived {
            node: x,
            edit: e,
            delete: d,
        });
    }
    // Revived (D-001): for each winning Applied Delete of `d` and each
    // concurrent Applied Move of `x != d`, `x` was under `d` in the state
    // before the earlier of the two, and `x` is live in the final
    // (post-revive) state (liveness condition (f)).
    let mut moves: Vec<(OpId, NodeId)> = Vec::new();
    for (&mid, mop) in &ops {
        if let (Payload::Move { node: x, .. }, Outcome::Applied) = (&mop.payload, &outcomes[&mid]) {
            moves.push((mid, *x));
        }
    }
    // (point, delete, move, d, x): the check runs on the state before `point`.
    let mut candidates: BTreeMap<OpId, Vec<(OpId, OpId, NodeId, NodeId)>> = BTreeMap::new();
    for (&did, dop) in &ops {
        let Payload::Delete { node: d } = dop.payload else {
            continue;
        };
        if outcomes[&did] != Outcome::Applied || applied[&d].last() != Some(&did) {
            continue;
        }
        for &(mid, x) in &moves {
            if x != d && is_live(&w.state, x) && causal.concurrent(did, mid) {
                candidates
                    .entry(did.min(mid))
                    .or_default()
                    .push((did, mid, d, x));
            }
        }
    }
    if !candidates.is_empty() {
        let mut w2 = Work::new();
        for (id, op) in &ops {
            if let Some(cs) = candidates.get(id) {
                for &(did, mid, d, x) in cs {
                    let first = &w2.state;
                    if first.contains_key(&x) && is_ancestor_or_self(first, d, x) {
                        conflicts.insert(Conflict::Revived {
                            node: x,
                            delete: did,
                            mv: mid,
                        });
                    }
                }
            }
            w2.apply(op);
        }
    }

    Evaluation {
        state: w.state,
        conflicts,
        outcomes,
        text,
    }
}

/// Pre-delete parent of a Delete whose node had `parent` just before it and
/// whose previous parent-setting op was `prev`: a re-delete of an already
/// deleted node (parent TRASH, `prev` a Delete) inherits `prev`'s pre-delete
/// parent, so a run of deletes restores to where the node was before the
/// first one. Deletes must be visited in ascending `<` order.
fn pre_delete_parent(parent: NodeId, prev: Option<OpId>, pre: &BTreeMap<OpId, NodeId>) -> NodeId {
    match prev.and_then(|p| pre.get(&p)) {
        Some(&pp) if parent == TRASH => pp,
        _ => parent,
    }
}

/// D-014 edit revive (Issue #20), shared by [`evaluate`] and [`Replica`].
///
/// Input: the sorted-fold `state`, `setter` (op that last set each node's
/// parent), `pre` (pre-delete parent per Applied Delete), the causality test
/// `precedes(a, b)` (`a ∈ deps*(b)`), and the Applied SetContent ops `edits`
/// as `(edit, node)`. For each edit `E` of
/// `x` with `x` not live: walk `x`'s chain to the node `d` whose parent is
/// TRASH; if `d`'s winning op is a `Delete` `D` with `E ∉ deps*(D)` (the
/// delete did not see the edit: concurrent, or `E` causally after `D`),
/// restore `d` under its pre-delete parent (name is unchanged by Delete) and
/// record `(x, E, D)`. Repeat until no change (a restored parent may itself
/// be deleted). A restore that would form a cycle, or whose pre-delete
/// parent is TRASH itself, is skipped. Deterministic
/// given the op set.
fn revive_by_edits(
    state: &mut State,
    ops: &BTreeMap<OpId, Op>,
    setter: &BTreeMap<NodeId, OpId>,
    pre: &BTreeMap<OpId, NodeId>,
    precedes: impl Fn(OpId, OpId) -> bool,
    edits: &[(OpId, NodeId)],
) -> BTreeSet<(NodeId, OpId, OpId)> {
    let mut out = BTreeSet::new();
    loop {
        let mut changed = false;
        for &(e, x) in edits {
            if is_live(state, x) {
                continue;
            }
            let mut top = x;
            let mut steps = 0;
            while let Some(p) = state[&top].parent {
                if p == TRASH || steps > state.len() {
                    break;
                }
                top = p;
                steps += 1;
            }
            if state[&top].parent != Some(TRASH) {
                continue;
            }
            let Some(&d) = setter.get(&top) else { continue };
            if !matches!(
                ops.get(&d).map(|o| &o.payload),
                Some(Payload::Delete { .. })
            ) || precedes(e, d)
            {
                continue;
            }
            let Some(&p) = pre.get(&d) else { continue };
            // Pre-delete parent TRASH (node was already directly in TRASH,
            // e.g. created or moved there): nothing to restore.
            if p == TRASH || is_ancestor_or_self(state, top, p) {
                continue;
            }
            state.get_mut(&top).unwrap().parent = Some(p);
            out.insert((x, e, d));
            changed = true;
        }
        if !changed {
            return out;
        }
    }
}

/// Whether the D-014 edit revive can change `st`: some Applied SetContent
/// (`Undo::Content`) targets a node that is not live.
fn needs_revive<'a>(st: &State, log: impl Iterator<Item = (&'a Op, &'a Undo)>) -> bool {
    let mut live: HashMap<NodeId, bool> = HashMap::new();
    for (o, u) in log {
        if let (Payload::SetContent { node, .. }, Undo::Content { .. }) = (&o.payload, u) {
            if !*live.entry(*node).or_insert_with(|| is_live(st, *node)) {
                return true;
            }
        }
    }
    false
}

/// A replica receiving ops in arbitrary order (possibly duplicated), buffering
/// until deps are present, applying incrementally with undo-do-redo (§2.3).
#[derive(Clone, Debug)]
pub struct Replica {
    work: Work,
    /// Applied ops in `<` order with their undo records and fold outcome
    /// (Issue #32: the outcome is what [`evaluate`] records for the op, so
    /// [`Replica::conflicts`] needs no second fold).
    log: Vec<(Op, Undo, Outcome)>,
    known: BTreeSet<OpId>,
    pending: BTreeMap<OpId, Op>,
    /// Issue #28 ready index: pending ops waiting on each missing dep and
    /// the number of missing deps per pending op (so draining is O(ops)
    /// instead of a scan of `pending` per op).
    waiting: HashMap<OpId, Vec<OpId>>,
    missing: HashMap<OpId, usize>,
    /// Fold state plus the D-014 edit revive (what [`Replica::state`]
    /// shows); `None` when the revive changes nothing (the common case).
    view: Option<State>,
    /// The `(node, edit, delete)` triples the last view refresh revived
    /// (the `EditRevived` conflicts of [`evaluate`]).
    revived: BTreeSet<(NodeId, OpId, OpId)>,
    /// Text-CRDT state (Issue #54); empty while no `TextEdit` is applied.
    text: TextPart,
}

/// Text part of a [`Replica`] (Issue #54, Text-CRDT 7.4): the per-epoch
/// docs and the 7.4.3/7.8 results per text node, maintained
/// incrementally: a new `TextEdit` is validated and integrated into its
/// doc on arrival (validity depends only on its causal past), and only
/// the nodes with new content ops are re-derived. When a batch changes the
/// fold outcome of an already applied text op or epoch op (an undo/redo
/// below them), the text part is rebuilt from the log.
#[derive(Clone, Debug, Default)]
pub(crate) struct TextPart {
    pub(crate) docs: BTreeMap<(NodeId, OpId), crate::text::DocSlot>,
    /// Valid (Applied) and text-invalid (structurally Applied) `TextEdit`s.
    pub(crate) valid: BTreeSet<OpId>,
    pub(crate) invalid: BTreeSet<OpId>,
    /// Per text node: winning epoch and head content id.
    pub(crate) content: BTreeMap<NodeId, (OpId, BlobId)>,
    pub(crate) conflicts: BTreeMap<NodeId, Vec<Conflict>>,
    pub(crate) notices: BTreeMap<NodeId, Vec<crate::text::Notice>>,
    /// Fold content of each node whose state content is its text head
    /// (restored before every fold step, so undo records stay fold values).
    pub(crate) fold: BTreeMap<NodeId, Option<BlobId>>,
    /// Some `TextEdit` is in the log (ops never leave it).
    pub(crate) any: bool,
    /// A lazy doc's seed blob (snapshot v2) that was missing or damaged
    /// when it was needed. Not snapshotted.
    pub(crate) seed_error: Option<BlobId>,
}

impl TextPart {
    pub(crate) fn is_empty(&self) -> bool {
        !self.any
    }
}

/// Blob bytes from epoch seeds already held, then from `next`.
struct SeedChain<'a> {
    seeds: HashMap<BlobId, Vec<u8>>,
    next: &'a dyn crate::text::BlobSource,
}

impl crate::text::BlobSource for SeedChain<'_> {
    fn blob_bytes(&self, id: &BlobId) -> Option<std::borrow::Cow<'_, [u8]>> {
        match self.seeds.get(id) {
            Some(b) => Some(std::borrow::Cow::Borrowed(b)),
            None => self.next.blob_bytes(id),
        }
    }
}

fn is_text_relevant(o: &Op) -> bool {
    matches!(
        o.payload,
        Payload::TextEdit(_) | Payload::SetContent { .. } | Payload::Create { .. }
    )
}

impl Default for Replica {
    fn default() -> Self {
        Self::new()
    }
}

impl Replica {
    pub fn new() -> Self {
        Replica {
            work: Work::new(),
            log: Vec::new(),
            known: BTreeSet::new(),
            pending: BTreeMap::new(),
            waiting: HashMap::new(),
            missing: HashMap::new(),
            view: None,
            revived: BTreeSet::new(),
            text: TextPart::default(),
        }
    }

    pub fn state(&self) -> &State {
        self.view.as_ref().unwrap_or(&self.work.state)
    }

    /// Recompute the view: the fold state with the D-014 revive applied
    /// (same function as [`evaluate`], inputs taken from the undo log).
    fn refresh_view(&mut self) {
        self.view = None;
        // Fast path: no Applied SetContent of a non-live file, nothing to do.
        let st = &self.work.state;
        let text_dead = self.text.content.keys().any(|n| !is_live(st, *n));
        if !text_dead && !needs_revive(st, self.log.iter().map(|(o, u, _)| (o, u))) {
            self.revived.clear();
            return;
        }
        let ops: BTreeMap<OpId, Op> = self.log.iter().map(|(o, _, _)| (o.id, o.clone())).collect();
        let causal = Causality::new(&ops);
        let mut pre = BTreeMap::new();
        let mut edits = Vec::new();
        for (o, u, _) in &self.log {
            match (&o.payload, u) {
                (Payload::Delete { .. }, Undo::Restore { parent, setter, .. }) => {
                    let parent = parent.expect("non-reserved node has parent");
                    let p = pre_delete_parent(parent, Some(*setter), &pre);
                    pre.insert(o.id, p);
                }
                (Payload::SetContent { node, .. }, Undo::Content { .. }) => {
                    edits.push((o.id, *node));
                }
                // A valid TextEdit is an edit (Text-CRDT 7.4.4).
                (Payload::TextEdit(t), _) if self.text.valid.contains(&o.id) => {
                    edits.push((o.id, t.node));
                }
                _ => {}
            }
        }
        edits.sort();
        let mut view = self.work.state.clone();
        self.revived = revive_by_edits(
            &mut view,
            &ops,
            &self.work.setter,
            &pre,
            |e, d| causal.precedes(e, d),
            &edits,
        );
        self.view = Some(view);
    }

    pub fn ops(&self) -> impl Iterator<Item = &Op> {
        self.log.iter().map(|(o, _, _)| o)
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Number of applied ops.
    pub fn len(&self) -> usize {
        self.log.len()
    }

    pub fn is_empty(&self) -> bool {
        self.log.is_empty()
    }

    /// The conflict set of the applied ops: equal to
    /// `evaluate(self.ops()).conflicts` (Issue #32), derived from the undo
    /// log and the view instead of a second full fold. `ops` must be the
    /// applied set keyed by id (e.g. the CLI's op store); it is only used to
    /// build [`Causality`] without cloning every op. Returns `None` if its
    /// keys differ from the applied set (the caller then uses [`evaluate`]).
    pub fn conflicts(&self, ops: &BTreeMap<OpId, Op>) -> Option<BTreeSet<Conflict>> {
        if ops.len() != self.log.len() || !ops.keys().eq(self.log.iter().map(|(o, _, _)| &o.id)) {
            return None;
        }
        let state = self.state();
        let causal = std::cell::OnceCell::new();
        let causal = || causal.get_or_init(|| Causality::new(ops));
        let mut conflicts = BTreeSet::new();
        // Fold conflicts: the logged outcome is the sorted-fold outcome.
        // Issue #33: log positions are the Causality indices (the store is
        // exactly the applied set, in the same `<` order).
        let mut applied: BTreeMap<NodeId, Vec<(OpId, usize)>> = BTreeMap::new();
        let mut sets: BTreeMap<NodeId, Vec<(OpId, BlobId, usize)>> = BTreeMap::new();
        let mut moves: Vec<(OpId, NodeId)> = Vec::new();
        for (pos, (op, _, out)) in self.log.iter().enumerate() {
            match out {
                Outcome::Invalid => {
                    conflicts.insert(Conflict::InvalidOp { op: op.id });
                }
                Outcome::Skipped { winner } => {
                    conflicts.insert(Conflict::Cycle {
                        winner: *winner,
                        loser: op.id,
                        node: op.moved_node().unwrap(),
                    });
                }
                Outcome::Applied if self.text.invalid.contains(&op.id) => {
                    conflicts.insert(Conflict::InvalidOp { op: op.id });
                }
                Outcome::Applied => {
                    if let Some(n) = op.moved_node() {
                        applied.entry(n).or_default().push((op.id, pos));
                    }
                    match &op.payload {
                        // Text nodes: 7.4.3 conflicts (Issue #54).
                        Payload::SetContent { node, .. }
                            if self.text.content.contains_key(node) => {}
                        Payload::SetContent { node, blob } => {
                            sets.entry(*node).or_default().push((op.id, *blob, pos))
                        }
                        Payload::Move { node, .. } => moves.push((op.id, *node)),
                        _ => {}
                    }
                }
            }
        }
        for (n, ms) in &applied {
            if ms.len() < 2 {
                continue;
            }
            let winner = ms.last().unwrap().0;
            let ps: Vec<usize> = ms.iter().map(|&(_, p)| p).collect();
            let over = causal().overwritten_at(&ps);
            for (i, &(m, _)) in ms[..ms.len() - 1].iter().enumerate() {
                if !over[i] {
                    conflicts.insert(Conflict::SameNodeMove {
                        winner,
                        loser: m,
                        node: *n,
                    });
                }
            }
        }
        for (n, es) in &sets {
            if es.len() < 2 {
                continue;
            }
            let (winner, winner_blob, _) = *es.last().unwrap();
            let ps: Vec<usize> = es.iter().map(|&(_, _, p)| p).collect();
            let over = causal().overwritten_at(&ps);
            for (i, &(e, loser_blob, _)) in es[..es.len() - 1].iter().enumerate() {
                if !over[i] {
                    conflicts.insert(Conflict::Content {
                        node: *n,
                        winner,
                        loser: e,
                        winner_blob,
                        loser_blob,
                    });
                }
            }
        }
        for cs in self.text.conflicts.values() {
            conflicts.extend(cs.iter().cloned());
        }
        for &(x, e, d) in &self.revived {
            conflicts.insert(Conflict::EditRevived {
                node: x,
                edit: e,
                delete: d,
            });
        }
        // Revived (D-001), as in `evaluate`: winning Applied Deletes against
        // concurrent Applied Moves of live nodes, checked on the fold state
        // before the earlier of the two.
        let mut candidates: BTreeMap<OpId, Vec<(OpId, OpId, NodeId, NodeId)>> = BTreeMap::new();
        if !moves.is_empty() {
            for (op, _, out) in &self.log {
                let Payload::Delete { node: d } = op.payload else {
                    continue;
                };
                let did = op.id;
                if *out != Outcome::Applied || applied[&d].last().map(|x| x.0) != Some(did) {
                    continue;
                }
                for &(mid, x) in &moves {
                    if x != d && is_live(state, x) && causal().concurrent(did, mid) {
                        candidates
                            .entry(did.min(mid))
                            .or_default()
                            .push((did, mid, d, x));
                    }
                }
            }
        }
        if !candidates.is_empty() {
            let mut w2 = Work::new();
            for (op, _, _) in &self.log {
                if let Some(cs) = candidates.get(&op.id) {
                    for &(did, mid, d, x) in cs {
                        let first = &w2.state;
                        if first.contains_key(&x) && is_ancestor_or_self(first, d, x) {
                            conflicts.insert(Conflict::Revived {
                                node: x,
                                delete: did,
                                mv: mid,
                            });
                        }
                    }
                }
                w2.apply(op);
            }
        }
        Some(conflicts)
    }

    /// The state [`Replica::state`] would show after receiving `tail`,
    /// without changing `self` (Issue #32: `hub record` plans the new head
    /// before appending). Only for a causally ready tail greater than every
    /// applied op, in `<` order, when no D-014 revive is involved; `None`
    /// otherwise (the caller then uses [`evaluate`]).
    pub fn state_with_tail(&self, tail: &[Op]) -> Option<State> {
        if !self.pending.is_empty() {
            return None;
        }
        let mut last = self.log.last().map(|(o, _, _)| o.id);
        let mut seen: BTreeSet<OpId> = BTreeSet::new();
        // Issue #54: a tail TextEdit needs the text pass (T3 authoring
        // plans with `receive` on a clone instead).
        if tail
            .iter()
            .any(|o| matches!(o.payload, Payload::TextEdit(_)))
        {
            return None;
        }
        for o in tail {
            if last.is_some_and(|l| o.id <= l)
                || !o
                    .deps
                    .iter()
                    .all(|d| self.known.contains(d) || seen.contains(d))
            {
                return None;
            }
            last = Some(o.id);
            seen.insert(o.id);
        }
        let mut w = Work {
            state: self.work.state.clone(),
            setter: self.work.setter.clone(),
        };
        let undo: Vec<Undo> = tail.iter().map(|o| w.apply(o).1).collect();
        let all = self
            .log
            .iter()
            .map(|(o, u, _)| (o, u))
            .chain(tail.iter().zip(undo.iter()));
        if needs_revive(&w.state, all) || self.text.content.keys().any(|n| !is_live(&w.state, *n)) {
            return None;
        }
        Some(w.state)
    }

    /// Receive an op. Duplicates are no-ops (§2.6 #6).
    pub fn receive(&mut self, op: Op) {
        self.receive_all(std::iter::once(op));
    }

    /// Receive a batch of ops (e.g. a whole op log on open), then refresh
    /// the view once (Issue #28). Same result as receiving them one by one.
    /// Issue #33: every op that becomes ready is integrated in one merged
    /// pass (undo the log suffix above the smallest new id once, then redo
    /// suffix and new ops in `<` order), so a batch of out-of-order ops
    /// costs O(suffix + batch) instead of O(suffix) per op.
    pub fn receive_all(&mut self, ops: impl IntoIterator<Item = Op>) {
        self.receive_all_with(ops, &crate::text::NoBlobs);
    }

    /// [`Replica::receive_all`] with epoch blob bytes for text ops (Issue
    /// #54, Text-CRDT 7.4.2): the seed of a new epoch doc is read from
    /// `blobs` (seeds of docs already held are kept). Equal to
    /// [`evaluate_with`] over the applied ops and the same blobs.
    pub fn receive_all_with(
        &mut self,
        ops: impl IntoIterator<Item = Op>,
        blobs: &dyn crate::text::BlobSource,
    ) {
        let mut batch = Vec::new();
        for op in ops {
            self.buffer(op, &mut batch);
        }
        let new: Vec<OpId> = batch.iter().map(|o| o.id).collect();
        self.restore_fold();
        let changed = self.integrate_all(batch);
        self.update_text(&new, changed, blobs);
        self.apply_fold();
        self.refresh_view();
    }

    /// Whether a doc for `(node, epoch)` exists (its seed is held).
    pub fn has_text_doc(&self, node: NodeId, epoch: OpId) -> bool {
        self.text.docs.contains_key(&(node, epoch))
    }

    /// Seeds of the docs a snapshot v2 referenced and that are not loaded
    /// yet: `(blob, length)` (the CLI checks they exist before trusting
    /// the snapshot).
    pub fn text_lazy_seeds(&self) -> Vec<(BlobId, u32)> {
        self.text
            .docs
            .values()
            .filter_map(|s| match s {
                crate::text::DocSlot::Lazy(d, b) => Some((*b, d.seed_len())),
                _ => None,
            })
            .collect()
    }

    /// Load every lazy doc's seed (tests, T3 materialization).
    pub fn load_text_docs(&mut self, blobs: &dyn crate::text::BlobSource) -> Result<(), BlobId> {
        for slot in self.text.docs.values_mut() {
            crate::text::load_slot(slot, blobs)?;
        }
        Ok(())
    }

    /// A referenced seed blob that was missing or damaged when needed.
    pub fn text_seed_error(&self) -> Option<BlobId> {
        self.text.seed_error
    }

    /// The doc of one epoch (`None` if absent, deferred or not loaded).
    pub fn text_doc(&self, node: NodeId, epoch: OpId) -> Option<&crate::text::TextDoc> {
        match self.text.docs.get(&(node, epoch)) {
            Some(crate::text::DocSlot::Doc(d)) => Some(d),
            _ => None,
        }
    }

    /// Per text node: winning epoch and head content id (7.4.3).
    pub fn text_content(&self) -> &BTreeMap<NodeId, (OpId, BlobId)> {
        &self.text.content
    }

    /// `concurrent-edit` notices (7.8), as `evaluate(..).text.notices`.
    pub fn text_notices(&self) -> BTreeSet<crate::text::Notice> {
        self.text.notices.values().flatten().cloned().collect()
    }

    /// Valid (Applied) `TextEdit`s, as `evaluate(..).text.valid`.
    pub fn text_valid(&self) -> &BTreeSet<OpId> {
        &self.text.valid
    }

    /// Load the lazy doc of one epoch (Issue #56: authoring and
    /// materialization touch only the docs they need). `Err(blob)` if its
    /// seed is missing or damaged.
    pub fn load_text_doc(
        &mut self,
        node: NodeId,
        epoch: OpId,
        blobs: &dyn crate::text::BlobSource,
    ) -> Result<(), BlobId> {
        match self.text.docs.get_mut(&(node, epoch)) {
            Some(slot) => crate::text::load_slot(slot, blobs),
            None => Ok(()),
        }
    }

    /// The winning epoch of `node` at the applied head and its blob
    /// (7.4.3), as [`crate::text::winning_epoch`] of
    /// `evaluate(self.ops())`: the text winner, else the greatest Applied
    /// `SetContent`, else the node's `Create` (empty) for a live file.
    pub fn content_epoch(&self, node: NodeId) -> Option<(OpId, BlobId)> {
        if let Some(&(epoch, _)) = self.text.content.get(&node) {
            return Some((epoch, self.epoch_blob_in(node, epoch)?));
        }
        for (o, _, out) in self.log.iter().rev() {
            if let Payload::SetContent { node: n, blob } = &o.payload {
                if *n == node && *out == Outcome::Applied {
                    return Some((o.id, *blob));
                }
            }
        }
        (self.logged(node).is_some() && self.state().get(&node).is_some_and(|n| n.kind == Kind::File))
            .then(|| (node, BlobId::empty()))
    }

    /// Content heads of `node` at the applied head (7.4.1 `seen`), as
    /// [`crate::text::content_heads`] of `evaluate(self.ops())`.
    pub fn content_heads(&self, node: NodeId) -> BTreeSet<OpId> {
        let cs: Vec<OpId> = self
            .log
            .iter()
            .filter(|(o, _, out)| match &o.payload {
                Payload::SetContent { node: n, .. } => *n == node && *out == Outcome::Applied,
                Payload::TextEdit(t) => t.node == node && self.text.valid.contains(&o.id),
                _ => false,
            })
            .map(|(o, _, _)| o.id)
            .collect();
        let Some(&min) = cs.first() else {
            return BTreeSet::new();
        };
        // Everything strictly before some content op: walk deps down to
        // the smallest content op (a dep is `<` its op, so nothing below
        // `min` can reach a content op).
        let mut below: BTreeSet<OpId> = BTreeSet::new();
        let mut stack: Vec<OpId> = Vec::new();
        for c in &cs {
            if let Some((o, _, _)) = self.logged(*c) {
                stack.extend(o.deps.iter().copied().filter(|d| *d >= min));
            }
        }
        while let Some(x) = stack.pop() {
            if !below.insert(x) {
                continue;
            }
            if let Some((o, _, _)) = self.logged(x) {
                stack.extend(o.deps.iter().copied().filter(|d| *d >= min));
            }
        }
        cs.into_iter().filter(|c| !below.contains(c)).collect()
    }

    /// Author a save of `node` against the applied head (7.5.1, Issue #56):
    /// [`crate::text::author_text_edit`] with `base_ops` = every applied
    /// op, without a full evaluation. The epoch's doc must be loaded
    /// ([`Replica::load_text_doc`]); `None` for a no-op save, an
    /// unavailable or non-UTF-8 epoch text, or a pending op.
    pub fn author_text_edit(
        &self,
        node: NodeId,
        saved: &[u8],
        blobs: &dyn crate::text::BlobSource,
    ) -> Option<crate::text::TextEdit> {
        if !self.pending.is_empty() {
            return None;
        }
        let (epoch, blob) = self.content_epoch(node)?;
        let fresh;
        let doc = match self.text.docs.get(&(node, epoch)) {
            Some(crate::text::DocSlot::Doc(d)) => &**d,
            Some(_) => return None,
            None => {
                let b = crate::text::fetch(blobs, &blob)?;
                std::str::from_utf8(&b).ok()?;
                fresh = crate::text::TextDoc::new(epoch, &b);
                &fresh
            }
        };
        let all = |_: OpId| true;
        let base = doc.visible_runs(&all);
        let (del, ins) = crate::text::diff_line(&base, saved, &|l| doc.successor_at(l, &all))?;
        Some(crate::text::TextEdit {
            node,
            epoch,
            seen: self.content_heads(node),
            del,
            ins,
        })
    }

    /// Bytes of a derived text blob this replica holds (Issue #56): a text
    /// node's head content, or the loser blob of a text content conflict
    /// (its epoch materialized at the loser's causal past). `None` if
    /// `id` is neither or its doc is not loaded.
    pub fn derived_blob(&self, id: &BlobId) -> Option<Vec<u8>> {
        for (node, (epoch, blob)) in &self.text.content {
            if blob == id {
                if let Some(d) = self.text_doc(*node, *epoch) {
                    return Some(d.materialize_head());
                }
            }
        }
        for cs in self.text.conflicts.values() {
            for c in cs {
                let Conflict::Content {
                    node,
                    loser,
                    loser_blob,
                    ..
                } = c
                else {
                    continue;
                };
                if loser_blob != id {
                    continue;
                }
                let Some((
                    Op {
                        payload: Payload::TextEdit(t),
                        ..
                    },
                    _,
                    _,
                )) = self.logged(*loser)
                else {
                    continue;
                };
                let Some(d) = self.text_doc(*node, t.epoch) else {
                    continue;
                };
                let mut past: HashSet<OpId> = HashSet::new();
                let mut stack = vec![*loser];
                while let Some(x) = stack.pop() {
                    if past.insert(x) {
                        if let Some((o, _, _)) = self.logged(x) {
                            stack.extend(o.deps.iter().copied());
                        }
                    }
                }
                let b = d.materialize(&|o| past.contains(&o));
                if BlobId::of(&b) == *id {
                    return Some(b);
                }
            }
        }
        None
    }

    fn logged(&self, id: OpId) -> Option<&(Op, Undo, Outcome)> {
        self.log
            .binary_search_by_key(&id, |(o, _, _)| o.id)
            .ok()
            .map(|i| &self.log[i])
    }

    /// [`crate::text::epoch_blob`] over the log.
    fn epoch_blob_in(&self, node: NodeId, epoch: OpId) -> Option<BlobId> {
        let (o, _, out) = self.logged(epoch)?;
        if *out != Outcome::Applied {
            return None;
        }
        match &o.payload {
            Payload::Create {
                kind: Kind::File, ..
            } if epoch == node => Some(BlobId::empty()),
            Payload::SetContent { node: n, blob } if *n == node => Some(*blob),
            _ => None,
        }
    }

    fn restore_fold(&mut self) {
        for (n, v) in std::mem::take(&mut self.text.fold) {
            if let Some(rec) = self.work.state.get_mut(&n) {
                rec.content = v;
            }
        }
    }

    fn apply_fold(&mut self) {
        for (n, &(_, blob)) in &self.text.content {
            if let Some(rec) = self.work.state.get_mut(n) {
                self.text.fold.insert(*n, rec.content);
                rec.content = Some(blob);
            }
        }
    }

    /// Bring the text part up to date after integrating `new` (ids of
    /// the ops just applied); `rebuild` = the fold outcome of an already
    /// applied text-relevant op changed.
    fn update_text(&mut self, new: &[OpId], rebuild: bool, blobs: &dyn crate::text::BlobSource) {
        let new_text: Vec<OpId> = {
            let mut v: Vec<OpId> = new
                .iter()
                .copied()
                .filter(|id| {
                    self.logged(*id)
                        .is_some_and(|(o, _, _)| matches!(o.payload, Payload::TextEdit(_)))
                })
                .collect();
            v.sort();
            v
        };
        if self.text.is_empty() && new_text.is_empty() {
            return;
        }
        self.text.any = true;
        let causal = Causality::of_sorted(self.log.iter().map(|(o, _, _)| o));
        let mut affected: BTreeSet<NodeId> = BTreeSet::new();
        let todo: Vec<OpId> = if rebuild {
            // Keep the seeds of the docs held, then start over.
            let mut seeds = HashMap::new();
            for ((node, epoch), slot) in &self.text.docs {
                if let (crate::text::DocSlot::Doc(d), Some(b)) =
                    (slot, self.epoch_blob_in(*node, *epoch))
                {
                    seeds.insert(b, d.seed().to_vec());
                }
            }
            self.text = TextPart {
                any: true,
                ..TextPart::default()
            };
            let src = SeedChain { seeds, next: blobs };
            let all: Vec<OpId> = self
                .log
                .iter()
                .filter(|(o, _, _)| matches!(o.payload, Payload::TextEdit(_)))
                .map(|(o, _, _)| o.id)
                .collect();
            self.integrate_text(&all, &causal, &src, &mut affected);
            Vec::new()
        } else {
            new_text
        };
        if !todo.is_empty() {
            self.integrate_text(&todo, &causal, blobs, &mut affected);
        }
        // A new Applied SetContent of a text node changes its winner.
        if !rebuild {
            for id in new {
                if let Some((
                    Op {
                        payload: Payload::SetContent { node, .. },
                        ..
                    },
                    _,
                    Outcome::Applied,
                )) = self.logged(*id)
                {
                    if self.text.content.contains_key(node) {
                        affected.insert(*node);
                    }
                }
            }
        }
        if affected.is_empty() {
            return;
        }
        let mut cs: BTreeMap<NodeId, Vec<OpId>> = BTreeMap::new();
        for (o, _, out) in &self.log {
            let n = match &o.payload {
                Payload::SetContent { node, .. } if *out == Outcome::Applied => *node,
                Payload::TextEdit(t) if self.text.valid.contains(&o.id) => t.node,
                _ => continue,
            };
            if affected.contains(&n) {
                cs.entry(n).or_default().push(o.id);
            }
        }
        // Every doc of an affected node is derived from: load lazy ones.
        for (k, slot) in self.text.docs.iter_mut() {
            if affected.contains(&k.0) {
                if let Err(b) = crate::text::load_slot(slot, blobs) {
                    self.text.seed_error = Some(b);
                }
            }
        }
        let get = |x: OpId| &self.logged(x).expect("op in log").0;
        let mut out = Vec::new();
        for (node, c) in &cs {
            out.push((
                *node,
                crate::text::derive_node(*node, c, &get, &causal, blobs, &self.text.docs),
            ));
        }
        for (node, d) in out {
            self.text
                .content
                .insert(node, (d.content.epoch, d.content.blob));
            self.text.conflicts.insert(node, d.conflicts);
            self.text.notices.insert(node, d.notices);
        }
    }

    /// [`crate::text::text_step`] for `ids` (ascending, so causal).
    fn integrate_text(
        &mut self,
        ids: &[OpId],
        causal: &Causality,
        blobs: &dyn crate::text::BlobSource,
        affected: &mut BTreeSet<NodeId>,
    ) {
        for &id in ids {
            let (op, _, out) = self.logged(id).expect("op in log");
            if *out != Outcome::Applied {
                continue;
            }
            let Payload::TextEdit(t) = &op.payload else {
                continue;
            };
            let t = t.clone();
            if let Some(slot) = self.text.docs.get_mut(&(t.node, t.epoch)) {
                if let Err(b) = crate::text::load_slot(slot, blobs) {
                    self.text.seed_error = Some(b);
                }
            }
            let eb = self.epoch_blob_in(t.node, t.epoch);
            let in_deps = |x: OpId| causal.precedes(x, id);
            if crate::text::text_step(id, &t, eb, &in_deps, &mut self.text.docs, blobs) {
                self.text.valid.insert(id);
                affected.insert(t.node);
            } else {
                self.text.invalid.insert(id);
            }
        }
    }

    /// Buffer one op: pending until every dep is known. A ready op is
    /// marked known and moved to `batch` (with every waiter it releases).
    fn buffer(&mut self, op: Op, batch: &mut Vec<Op>) {
        if self.known.contains(&op.id) || self.pending.contains_key(&op.id) {
            return;
        }
        let id = op.id;
        let mut n = 0;
        for d in &op.deps {
            if !self.known.contains(d) {
                self.waiting.entry(*d).or_default().push(id);
                n += 1;
            }
        }
        if n > 0 {
            self.pending.insert(id, op);
            self.missing.insert(id, n);
            return;
        }
        // Ready: known now; release the waiters it unblocks.
        self.known.insert(id);
        batch.push(op);
        let mut stack = vec![id];
        while let Some(done) = stack.pop() {
            for w in self.waiting.remove(&done).unwrap_or_default() {
                let m = self.missing.get_mut(&w).expect("waiter is pending");
                *m -= 1;
                if *m == 0 {
                    self.missing.remove(&w);
                    let o = self.pending.remove(&w).expect("waiter is pending");
                    self.known.insert(w);
                    batch.push(o);
                    stack.push(w);
                }
            }
        }
    }

    /// Integrate ready ops (ids new, deps known): undo the log above the
    /// smallest, then apply suffix and batch merged in `<` order.
    /// Returns whether the fold outcome of a redone text-relevant op
    /// (`TextEdit`, `SetContent`, `Create`) changed (Issue #54).
    fn integrate_all(&mut self, mut batch: Vec<Op>) -> bool {
        if batch.is_empty() {
            return false;
        }
        batch.sort_by_key(|o| o.id);
        let pos = self.log.partition_point(|(o, _, _)| o.id < batch[0].id);
        let tail: Vec<(Op, Undo, Outcome)> = self.log.drain(pos..).collect();
        let mut redo = Vec::with_capacity(tail.len());
        let track = !self.text.is_empty();
        let mut before: HashMap<OpId, Outcome> = HashMap::new();
        for (o, u, out) in tail.into_iter().rev() {
            self.work.undo(u);
            if track && is_text_relevant(&o) {
                before.insert(o.id, out);
            }
            redo.push(o);
        }
        redo.reverse();
        self.log.reserve(redo.len() + batch.len());
        let mut a = redo.into_iter().peekable();
        let mut b = batch.into_iter().peekable();
        loop {
            let next = match (a.peek(), b.peek()) {
                (Some(x), Some(y)) => {
                    if x.id < y.id {
                        a.next()
                    } else {
                        b.next()
                    }
                }
                (Some(_), None) => a.next(),
                (None, Some(_)) => b.next(),
                (None, None) => break,
            }
            .unwrap();
            let (out, u) = self.work.apply(&next);
            self.log.push((next, u, out));
        }
        before
            .iter()
            .any(|(id, out)| self.logged(*id).is_some_and(|(_, _, o)| o != out))
    }
}

/// Check §1.3 invariants 1-4 and 6 on `state`, given the op set producing it.
pub fn check_invariants<'a>(
    state: &State,
    ops: impl IntoIterator<Item = &'a Op>,
    outcomes: &BTreeMap<OpId, Outcome>,
) -> Result<(), String> {
    // 1 single root; TRASH the only other parentless node.
    for n in state.values() {
        if n.kind == Kind::Root && n.id != ROOT {
            return Err(format!("extra root {:?}", n.id));
        }
        if n.parent.is_none() && n.id != ROOT && n.id != TRASH {
            return Err(format!("parentless {:?}", n.id));
        }
    }
    match (state.get(&ROOT), state.get(&TRASH)) {
        (Some(r), Some(t))
            if r.kind == Kind::Root
                && r.parent.is_none()
                && t.kind == Kind::Directory
                && t.parent.is_none() => {}
        _ => return Err("ROOT/TRASH malformed".into()),
    }
    for n in state.values() {
        if n.id == ROOT || n.id == TRASH {
            continue;
        }
        // 2 parent exists and is Root/Directory.
        let p = n.parent.unwrap();
        match state.get(&p).map(|p| p.kind) {
            Some(Kind::Root | Kind::Directory) => {}
            _ => return Err(format!("bad parent of {:?}", n.id)),
        }
        // 3 reaches ROOT or TRASH within |nodes| steps.
        let mut cur = n.id;
        let mut steps = 0;
        while cur != ROOT && cur != TRASH {
            cur = state[&cur].parent.unwrap();
            steps += 1;
            if steps > state.len() {
                return Err(format!("cycle through {:?}", n.id));
            }
        }
        // Content present exactly on Files (Issue #18).
        if (n.kind == Kind::File) != n.content.is_some() {
            return Err(format!("content/kind mismatch on {:?}", n.id));
        }
        // 6 name validity (checked for all records, stronger than live only).
        if !valid_name(&n.name) {
            return Err(format!("bad name on {:?}", n.id));
        }
    }
    // 4 node set = {ROOT, TRASH} ∪ valid creates.
    let mut expect: BTreeSet<NodeId> = [ROOT, TRASH].into();
    for o in ops {
        if matches!(o.payload, Payload::Create { .. })
            && outcomes.get(&o.id) == Some(&Outcome::Applied)
        {
            expect.insert(o.id);
        }
    }
    let got: BTreeSet<NodeId> = state.keys().copied().collect();
    if got != expect {
        return Err(format!("node set mismatch {got:?} vs {expect:?}"));
    }
    Ok(())
}

#[cfg(any(test, feature = "oracle"))]
#[path = "tree_oracle.rs"]
pub mod oracle;

#[path = "tree_snapshot.rs"]
mod snapshot;
