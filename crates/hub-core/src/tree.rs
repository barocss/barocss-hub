//! In-memory replicated tree (Repository-Semantics §1-§2, structural ops only).
//!
//! The spec is `State(Ops) = fold(apply, S0, sort(Ops by <))` (§2.3). [`evaluate`]
//! implements that fold and derives the conflict set; [`Replica`] implements the
//! incremental undo-do-redo optimization with causal buffering and duplicate
//! suppression, and must agree with [`evaluate`].

use std::collections::{BTreeMap, BTreeSet};

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
            Payload::Create { .. } => None,
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
        },
    );
    s.insert(
        TRASH,
        Node {
            id: TRASH,
            kind: Kind::Directory,
            parent: None,
            name: String::new(),
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
                    },
                );
                self.setter.insert(op.id, op.id);
                (Outcome::Applied, Undo::RemoveNode(op.id))
            }
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
                    Payload::Create { .. } => unreachable!(),
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
    // Revived (Issue #5 trigger): D ∥ M, some d strict ancestor of x just
    // before min(D, M), x live finally, and d's winning op is D.
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

/// A replica receiving ops in arbitrary order (possibly duplicated), buffering
/// until deps are present, applying incrementally with undo-do-redo (§2.3).
#[derive(Clone, Debug)]
pub struct Replica {
    work: Work,
    /// Applied ops in `<` order with their undo records.
    log: Vec<(Op, Undo)>,
    known: BTreeSet<OpId>,
    pending: BTreeMap<OpId, Op>,
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
        }
    }

    pub fn state(&self) -> &State {
        &self.work.state
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
