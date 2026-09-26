//! In-memory replicated tree (Repository-Semantics §1-§2, structural ops only).
//!
//! The spec is `State(Ops) = fold(apply, S0, sort(Ops by <))` (§2.3). [`evaluate`]
//! implements that fold and derives the conflict set; [`Replica`] implements the
//! incremental undo-do-redo optimization with causal buffering and duplicate
//! suppression, and must agree with [`evaluate`].

use std::collections::{BTreeMap, BTreeSet};

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
        self.blobs.contains_key(id)
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

/// The specification: sorted fold (§2.3) plus derived conflicts (§2.4, §2.5).
pub fn evaluate<'a>(ops: impl IntoIterator<Item = &'a Op>) -> Evaluation {
    let ops: BTreeMap<OpId, Op> = ops.into_iter().map(|o| (o.id, o.clone())).collect();
    let past = causal_past(&ops);
    let concurrent = |a: OpId, b: OpId| !past[&a].contains(&b) && !past[&b].contains(&a);

    let mut w = Work::new();
    let mut outcomes = BTreeMap::new();
    let mut conflicts = BTreeSet::new();
    let mut before: BTreeMap<OpId, State> = BTreeMap::new();
    for (id, op) in &ops {
        if op.moved_node().is_some() {
            before.insert(*id, w.state.clone());
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
        for &m in &ms[..ms.len() - 1] {
            let overwritten_causally = ms.iter().any(|&m2| past[&m2].contains(&m));
            if !overwritten_causally {
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
        for &(e, loser_blob) in &es[..es.len() - 1] {
            if !es.iter().any(|&(e2, _)| past[&e2].contains(&e)) {
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
    for (id, st) in &before {
        if let (Payload::Delete { node }, Outcome::Applied) = (&ops[id].payload, &outcomes[id]) {
            let ms = &applied[node];
            let prev = ms
                .iter()
                .position(|m| m == id)
                .and_then(|i| i.checked_sub(1));
            let prev = prev.map(|i| ms[i]);
            let parent = st[node].parent.expect("non-reserved node has parent");
            pre.insert(*id, pre_delete_parent(parent, prev, &pre));
        }
    }
    for (x, e, d) in revive_by_edits(&mut w.state, &ops, &w.setter, &pre, &past, &edits) {
        conflicts.insert(Conflict::EditRevived {
            node: x,
            edit: e,
            delete: d,
        });
    }
    // Revived (D-001) liveness condition (f) is evaluated on the final
    // (post-revive) state.
    for (&did, dop) in &ops {
        let Payload::Delete { node: d } = dop.payload else {
            continue;
        };
        if outcomes[&did] != Outcome::Applied || applied[&d].last() != Some(&did) {
            continue;
        }
        for (&mid, mop) in &ops {
            let Payload::Move { node: x, .. } = mop.payload else {
                continue;
            };
            if x == d || outcomes[&mid] != Outcome::Applied || !concurrent(did, mid) {
                continue;
            }
            let first = &before[&did.min(mid)];
            if first.contains_key(&x) && is_ancestor_or_self(first, d, x) && is_live(&w.state, x) {
                conflicts.insert(Conflict::Revived {
                    node: x,
                    delete: did,
                    mv: mid,
                });
            }
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
/// parent), `pre` (pre-delete parent per Applied Delete), causal `past`, and
/// the Applied SetContent ops `edits` as `(edit, node)`. For each edit `E` of
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
    past: &BTreeMap<OpId, BTreeSet<OpId>>,
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
            ) || past[&d].contains(&e)
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

/// A replica receiving ops in arbitrary order (possibly duplicated), buffering
/// until deps are present, applying incrementally with undo-do-redo (§2.3).
#[derive(Clone, Debug)]
pub struct Replica {
    work: Work,
    /// Applied ops in `<` order with their undo records.
    log: Vec<(Op, Undo)>,
    known: BTreeSet<OpId>,
    pending: BTreeMap<OpId, Op>,
    /// Fold state plus the D-014 edit revive (what [`Replica::state`]
    /// shows); `None` when the revive changes nothing (the common case).
    view: Option<State>,
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
            view: None,
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
        let needed = self.log.iter().any(|(o, u)| {
            matches!((&o.payload, u), (Payload::SetContent { node, .. }, Undo::Content { .. }) if !is_live(st, *node))
        });
        if !needed {
            return;
        }
        let ops: BTreeMap<OpId, Op> = self.log.iter().map(|(o, _)| (o.id, o.clone())).collect();
        let past = causal_past(&ops);
        let mut pre = BTreeMap::new();
        let mut edits = Vec::new();
        for (o, u) in &self.log {
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
        revive_by_edits(&mut view, &ops, &self.work.setter, &pre, &past, &edits);
        self.view = Some(view);
    }

    pub fn ops(&self) -> impl Iterator<Item = &Op> {
        self.log.iter().map(|(o, _)| o)
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Receive an op. Duplicates are no-ops (§2.6 #6).
    pub fn receive(&mut self, op: Op) {
        if self.known.contains(&op.id) || self.pending.contains_key(&op.id) {
            return;
        }
        self.pending.insert(op.id, op);
        loop {
            let ready = self
                .pending
                .values()
                .find(|o| o.deps.iter().all(|d| self.known.contains(d)))
                .map(|o| o.id);
            let Some(id) = ready else { break };
            let op = self.pending.remove(&id).unwrap();
            self.integrate(op);
        }
        self.refresh_view();
    }

    fn integrate(&mut self, op: Op) {
        let pos = self.log.partition_point(|(o, _)| o.id < op.id);
        let tail: Vec<(Op, Undo)> = self.log.drain(pos..).collect();
        let mut redo = Vec::with_capacity(tail.len());
        for (o, u) in tail.into_iter().rev() {
            self.work.undo(u);
            redo.push(o);
        }
        self.known.insert(op.id);
        let (_, u) = self.work.apply(&op);
        self.log.push((op, u));
        for o in redo.into_iter().rev() {
            let (_, u) = self.work.apply(&o);
            self.log.push((o, u));
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
