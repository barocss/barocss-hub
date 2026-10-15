//! Test-only oracle (Issue #28): the pre-#28 `evaluate` and D-014 revive,
//! verbatim except for paths, built on the materialized [`causal_past`].
//! O(n²) time and memory on a linear history. Compiled only with the
//! `oracle` feature (enabled for this crate's own tests via a self
//! dev-dependency) so the equivalence proptest can compare the scalable
//! [`super::evaluate`] against it. Not for production use.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    causal_past, is_ancestor_or_self, is_live, pre_delete_parent, BlobId, Conflict, Evaluation,
    NodeId, Op, OpId, Outcome, Payload, State, Work, TRASH,
};

/// The specification: sorted fold (§2.3) plus derived conflicts (§2.4, §2.5).
pub fn evaluate<'a>(ops: impl IntoIterator<Item = &'a Op>) -> Evaluation {
    evaluate_text_aware(ops, &BTreeSet::new())
}

/// [`evaluate`] for op sets with text ops (Issue #53 oracle): `valid` =
/// the TextEdits valid per Text-CRDT 7.4.1/7.4.2 (decided by the caller);
/// every other structurally applicable TextEdit is `invalid-op`; valid
/// ones are edits for the D-014 revive; nodes with valid ones get no
/// SetContent-only content conflicts (the caller derives 7.4.3).
pub fn evaluate_text_aware<'a>(
    ops: impl IntoIterator<Item = &'a Op>,
    valid: &BTreeSet<OpId>,
) -> Evaluation {
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
        let (mut out, _) = w.apply(op);
        if matches!(op.payload, Payload::TextEdit(_))
            && out == Outcome::Applied
            && !valid.contains(id)
        {
            out = Outcome::Invalid;
        }
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
    let text_nodes: BTreeSet<NodeId> = valid
        .iter()
        .filter_map(|v| match &ops.get(v)?.payload {
            Payload::TextEdit(t) => Some(t.node),
            _ => None,
        })
        .collect();
    for (n, es) in &sets {
        if text_nodes.contains(n) {
            continue;
        }
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
    for v in valid {
        if let Some(Payload::TextEdit(t)) = ops.get(v).map(|o| &o.payload) {
            if outcomes.get(v) == Some(&Outcome::Applied) {
                edits.push((*v, t.node));
            }
        }
    }
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
        text: Default::default(),
    }
}

/// D-014 edit revive (Issue #20), pre-#28 form taking the materialized `past`.
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
