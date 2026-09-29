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
            Payload::Create { .. } | Payload::SetContent { .. } => None,
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

pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
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
                    Payload::Create { .. } | Payload::SetContent { .. } => unreachable!(),
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
        let ids: Vec<OpId> = ops.keys().copied().collect();
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
        for (i, op) in ops.values().enumerate() {
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
    let ops: BTreeMap<OpId, Op> = ops.into_iter().map(|o| (o.id, o.clone())).collect();
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
    // Edit revives (D-014, Issue #20): post-fold, order-independent.
    let mut edits: Vec<(OpId, NodeId)> = sets
        .iter()
        .flat_map(|(n, es)| es.iter().map(move |&(e, _)| (e, *n)))
        .collect();
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
        if !needs_revive(st, self.log.iter().map(|(o, u, _)| (o, u))) {
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
                Outcome::Applied => {
                    if let Some(n) = op.moved_node() {
                        applied.entry(n).or_default().push((op.id, pos));
                    }
                    match &op.payload {
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
        if needs_revive(&w.state, all) {
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
        let mut batch = Vec::new();
        for op in ops {
            self.buffer(op, &mut batch);
        }
        self.integrate_all(batch);
        self.refresh_view();
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
    fn integrate_all(&mut self, mut batch: Vec<Op>) {
        if batch.is_empty() {
            return;
        }
        batch.sort_by_key(|o| o.id);
        let pos = self.log.partition_point(|(o, _, _)| o.id < batch[0].id);
        let tail: Vec<(Op, Undo, Outcome)> = self.log.drain(pos..).collect();
        let mut redo = Vec::with_capacity(tail.len());
        for (o, u, _) in tail.into_iter().rev() {
            self.work.undo(u);
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
