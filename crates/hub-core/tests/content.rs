//! Issue #18: file content v0 (SetContent, whole-file blobs, content conflicts).

use std::collections::{BTreeMap, BTreeSet};

use hub_core::projection::project;
use hub_core::tree::*;
use proptest::prelude::*;

mod common;
use common::*;

fn content_of(s: &State) -> BTreeMap<NodeId, Option<BlobId>> {
    s.iter().map(|(k, n)| (*k, n.content)).collect()
}

fn content_conflicts(ev: &Evaluation) -> BTreeSet<Conflict> {
    ev.conflicts
        .iter()
        .filter(|c| matches!(c, Conflict::Content { .. } | Conflict::EditRevived { .. }))
        .cloned()
        .collect()
}

/// No silent drop for SetContent: every Applied SetContent is the winner of
/// its node, or overwritten by a causally later Applied SetContent (zero
/// losses), or the loser of exactly one `content` conflict. Invalid ones are
/// only `invalid-op`.
fn content_no_silent_drop(ops: &[Op], ev: &Evaluation) -> Result<(), String> {
    let map: BTreeMap<OpId, Op> = ops.iter().map(|o| (o.id, o.clone())).collect();
    let past = causal_past(&map);
    for op in ops {
        let Some(n) = op.content_node() else { continue };
        let losses = ev
            .conflicts
            .iter()
            .filter(|c| matches!(c, Conflict::Content { loser, .. } if *loser == op.id))
            .count();
        if ev.outcomes[&op.id] == Outcome::Invalid {
            if losses != 0 || !ev.conflicts.contains(&Conflict::InvalidOp { op: op.id }) {
                return Err(format!("invalid {:?} mis-recorded", op.id));
            }
            continue;
        }
        let applied: Vec<&Op> = ops
            .iter()
            .filter(|o| o.content_node() == Some(n) && ev.outcomes[&o.id] == Outcome::Applied)
            .collect();
        let winner = applied.iter().map(|o| o.id).max() == Some(op.id);
        let causal = applied.iter().any(|o| past[&o.id].contains(&op.id));
        let want = if winner || causal { 0 } else { 1 };
        if losses != want {
            return Err(format!(
                "{:?}: winner={winner} causal={causal} losses={losses}",
                op.id
            ));
        }
        if winner {
            let Payload::SetContent { blob, .. } = op.payload else {
                unreachable!()
            };
            if ev.state[&n].content != Some(blob) {
                return Err(format!("winner {:?} blob not in state", op.id));
            }
        }
    }
    Ok(())
}

/// D-014: no Applied SetContent of a non-live file is left whose trashing
/// Delete did not see it (else the edit should have revived), and every
/// `edit-revived` names an Applied edit of that node and a Delete that did
/// not causally follow it.
fn edit_revive_holds(ops: &[Op], ev: &Evaluation) -> Result<(), String> {
    let map: BTreeMap<OpId, Op> = ops.iter().map(|o| (o.id, o.clone())).collect();
    let past = causal_past(&map);
    for op in ops {
        let Some(x) = op.content_node() else { continue };
        if ev.outcomes[&op.id] != Outcome::Applied || is_live(&ev.state, x) {
            continue;
        }
        let mut top = x;
        while let Some(p) = ev.state[&top].parent {
            if p == TRASH {
                break;
            }
            top = p;
        }
        // The winning parent-setting op of `top`: the greatest Applied
        // Move/Delete of it.
        let w = ops
            .iter()
            .filter(|o| o.moved_node() == Some(top) && ev.outcomes[&o.id] == Outcome::Applied)
            .map(|o| o.id)
            .max();
        if let Some(w) = w {
            if matches!(map[&w].payload, Payload::Delete { .. }) && !past[&w].contains(&op.id) {
                // Exempt: pre-delete parent is TRASH itself, or the restore
                // would form a cycle.
                let prefix: Vec<&Op> = ops.iter().filter(|o| o.id < w).collect();
                let p = evaluate(prefix).state[&top].parent.unwrap();
                if p == TRASH || is_ancestor_or_self(&ev.state, top, p) {
                    continue;
                }
                return Err(format!(
                    "edit {:?} of dead {:?} did not revive (delete {:?})",
                    op.id, x, w
                ));
            }
        }
    }
    for c in &ev.conflicts {
        if let Conflict::EditRevived { node, edit, delete } = c {
            if map[edit].content_node() != Some(*node)
                || !matches!(map[delete].payload, Payload::Delete { .. })
                || past[delete].contains(edit)
            {
                return Err(format!("bad {c:?}"));
            }
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    /// Content state and content conflicts identical across replicas under
    /// shuffled + duplicated delivery; each loser has exactly one conflict.
    #[test]
    fn content_converges_and_no_silent_drop((nrep, script, seeds) in script_strategy()) {
        let ops = simulate(nrep, &script);
        let spec = evaluate(&ops);
        prop_assert!(check_invariants(&spec.state, &ops, &spec.outcomes).is_ok());
        for &seed in &seeds {
            let mut rep = Replica::new();
            for op in delivery(&ops, seed) {
                rep.receive(op);
            }
            prop_assert_eq!(content_of(rep.state()), content_of(&spec.state));
            let ev = evaluate(rep.ops());
            prop_assert_eq!(content_conflicts(&ev), content_conflicts(&spec));
            prop_assert_eq!(project(rep.state()).blobs, project(&spec.state).blobs);
        }
        let r = content_no_silent_drop(&ops, &spec);
        prop_assert!(r.is_ok(), "{:?}", r);
        let r = edit_revive_holds(&ops, &spec);
        prop_assert!(r.is_ok(), "{:?}", r);
    }

    /// A SetContent causally after every head clears all content conflicts
    /// of that node and becomes its content.
    #[test]
    fn causally_later_set_content_clears((nrep, script, _seeds) in script_strategy()) {
        let mut ops = simulate(nrep, &script);
        let ev = evaluate(&ops);
        let Some(n) = ev.conflicts.iter().find_map(|c| match c {
            Conflict::Content { node, .. } => Some(*node),
            _ => None,
        }) else { return Ok(()) };
        let heads: BTreeSet<OpId> = {
            let mut h: BTreeSet<OpId> = ops.iter().map(|o| o.id).collect();
            for o in &ops { for d in &o.deps { h.remove(d); } }
            h
        };
        let max = ops.iter().map(|o| o.counter_of()).max().unwrap();
        let blob = BlobId::of(b"resolved");
        ops.push(Op { id: NodeId::new(max + 1, 9), deps: heads, payload: Payload::SetContent { node: n, blob } });
        let ev2 = evaluate(&ops);
        let cleared = !ev2.conflicts.iter().any(|c| matches!(c, Conflict::Content { node, .. } if *node == n));
        prop_assert!(cleared);
        prop_assert_eq!(ev2.state[&n].content, Some(blob));
    }
}

trait Counter {
    fn counter_of(&self) -> u64;
}
impl Counter for Op {
    fn counter_of(&self) -> u64 {
        self.id.counter
    }
}

// ---------- explicit scenarios ----------

const A: u128 = 1;
const B: u128 = 2;
const C: u128 = 3;

fn op(counter: u64, rid: u128, deps: &[NodeId], payload: Payload) -> Op {
    Op {
        id: NodeId::new(counter, rid),
        deps: deps.iter().copied().collect(),
        payload,
    }
}

/// `/a/`, `/b/`, `/a/x` (File) by A at counters 2..4.
fn base() -> (Vec<Op>, NodeId, NodeId, NodeId) {
    let a = NodeId::new(2, A);
    let b = NodeId::new(3, A);
    let x = NodeId::new(4, A);
    let mk = |id: NodeId, deps: &[NodeId], kind, parent, name: &str| {
        op(
            id.counter,
            A,
            deps,
            Payload::Create {
                kind,
                parent,
                name: name.into(),
            },
        )
    };
    let ops = vec![
        mk(a, &[], Kind::Directory, ROOT, "a"),
        mk(b, &[a], Kind::Directory, ROOT, "b"),
        mk(x, &[b], Kind::File, a, "x"),
    ];
    (ops, a, b, x)
}

fn set(counter: u64, rid: u128, deps: &[NodeId], node: NodeId, bytes: &[u8]) -> Op {
    op(
        counter,
        rid,
        deps,
        Payload::SetContent {
            node,
            blob: BlobId::of(bytes),
        },
    )
}

fn all_orders_agree(ops: &[Op]) -> Evaluation {
    let spec = evaluate(ops);
    for seed in 1..20u64 {
        let mut rep = Replica::new();
        for o in delivery(ops, seed) {
            rep.receive(o);
        }
        assert_eq!(rep.state(), &spec.state);
        assert_eq!(evaluate(rep.ops()).conflicts, spec.conflicts);
    }
    spec
}

#[test]
fn new_file_has_empty_blob_and_store_roundtrip() {
    let (ops, a, _, x) = base();
    let ev = evaluate(&ops);
    assert_eq!(ev.state[&x].content, Some(BlobId::empty()));
    assert_eq!(ev.state[&a].content, None);
    assert_eq!(
        project(&ev.state).blobs,
        BTreeMap::from([(x, BlobId::empty())])
    );
    let mut st = BlobStore::new();
    let id = st.put(b"hello");
    assert_eq!(id, BlobId::of(b"hello"));
    assert_eq!(st.get(&id), Some(&b"hello"[..]));
    assert_eq!(
        BlobId::empty().hex(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn two_concurrent_edits() {
    let (mut ops, _, _, x) = base();
    ops.push(set(5, A, &[x], x, b"A"));
    ops.push(set(5, B, &[x], x, b"B"));
    let ev = all_orders_agree(&ops);
    assert_eq!(ev.state[&x].content, Some(BlobId::of(b"B")));
    assert_eq!(
        content_conflicts(&ev),
        BTreeSet::from([Conflict::Content {
            node: x,
            winner: NodeId::new(5, B),
            loser: NodeId::new(5, A),
            winner_blob: BlobId::of(b"B"),
            loser_blob: BlobId::of(b"A"),
        }])
    );
}

#[test]
fn three_concurrent_edits_one_conflict_per_loser() {
    let (mut ops, _, _, x) = base();
    ops.push(set(5, A, &[x], x, b"A"));
    ops.push(set(5, B, &[x], x, b"B"));
    ops.push(set(5, C, &[x], x, b"C"));
    let ev = all_orders_agree(&ops);
    assert_eq!(ev.state[&x].content, Some(BlobId::of(b"C")));
    let w = NodeId::new(5, C);
    let cs = content_conflicts(&ev);
    assert_eq!(cs.len(), 2);
    for (l, lb) in [(NodeId::new(5, A), b"A"), (NodeId::new(5, B), b"B")] {
        assert!(cs.contains(&Conflict::Content {
            node: x,
            winner: w,
            loser: l,
            winner_blob: BlobId::of(b"C"),
            loser_blob: BlobId::of(lb),
        }));
    }
}

#[test]
fn causally_later_edit_is_not_a_conflict_and_clears() {
    let (mut ops, _, _, x) = base();
    let ea = set(5, A, &[x], x, b"A");
    let eb = set(5, B, &[x], x, b"B");
    let merge = set(6, A, &[ea.id, eb.id], x, b"M");
    ops.extend([ea, eb, merge]);
    let ev = all_orders_agree(&ops);
    assert_eq!(ev.state[&x].content, Some(BlobId::of(b"M")));
    assert!(content_conflicts(&ev).is_empty());
}

// ---------- D-014: an edit revives a deleted file (Issue #20) ----------

#[test]
fn edit_vs_delete_both_orders_revives() {
    for (erid, drid) in [(A, B), (B, A)] {
        let (mut ops, a, _, x) = base();
        let e = set(5, erid, &[x], x, b"E");
        let d = op(5, drid, &[x], Payload::Delete { node: x });
        ops.extend([e.clone(), d.clone()]);
        let ev = all_orders_agree(&ops);
        // Revived to its pre-delete parent `a` with the edited content.
        assert!(is_live(&ev.state, x));
        assert_eq!(ev.state[&x].parent, Some(a));
        assert_eq!(project(&ev.state).paths[&x], "a/x");
        assert_eq!(project(&ev.state).blobs[&x], BlobId::of(b"E"));
        assert_eq!(
            content_conflicts(&ev),
            BTreeSet::from([Conflict::EditRevived {
                node: x,
                edit: e.id,
                delete: d.id
            }])
        );
        assert!(check_invariants(&ev.state, &ops, &ev.outcomes).is_ok());
    }
}

#[test]
fn edit_vs_delete_of_parent_folder_revives_folder() {
    for (erid, drid) in [(A, B), (B, A)] {
        let (mut ops, a, _, x) = base();
        let e = set(5, erid, &[x], x, b"E");
        let d = op(5, drid, &[x], Payload::Delete { node: a });
        ops.extend([e.clone(), d.clone()]);
        let ev = all_orders_agree(&ops);
        // The deleted node (`a`) returns to its pre-delete parent; x's path
        // is unchanged.
        assert!(is_live(&ev.state, a));
        assert_eq!(ev.state[&a].parent, Some(ROOT));
        assert_eq!(project(&ev.state).paths[&x], "a/x");
        assert_eq!(
            content_conflicts(&ev),
            BTreeSet::from([Conflict::EditRevived {
                node: x,
                edit: e.id,
                delete: d.id
            }])
        );
    }
}

#[test]
fn edit_vs_nested_deletes_revives_whole_chain() {
    // A deletes `a`; B (after seeing a move of `a` under `b`) ... simpler:
    // `c` under `a`, x under `c`; A deletes `c`, B deletes `a`, C edits x,
    // all concurrent: both deletes are undone so x is live again.
    let (mut ops, a, _, x) = base();
    let c = NodeId::new(5, A);
    ops.push(op(
        5,
        A,
        &[x],
        Payload::Create {
            kind: Kind::Directory,
            parent: a,
            name: "c".into(),
        },
    ));
    ops.push(op(
        6,
        A,
        &[c],
        Payload::Move {
            node: x,
            new_parent: c,
            new_name: "x".into(),
        },
    ));
    let m = NodeId::new(6, A);
    let e = set(7, C, &[m], x, b"E");
    let d1 = op(7, A, &[m], Payload::Delete { node: c });
    let d2 = op(7, B, &[m], Payload::Delete { node: a });
    ops.extend([e.clone(), d1.clone(), d2.clone()]);
    let ev = all_orders_agree(&ops);
    assert_eq!(project(&ev.state).paths[&x], "a/c/x");
    assert_eq!(
        content_conflicts(&ev),
        BTreeSet::from([
            Conflict::EditRevived {
                node: x,
                edit: e.id,
                delete: d1.id
            },
            Conflict::EditRevived {
                node: x,
                edit: e.id,
                delete: d2.id
            },
        ])
    );
}

#[test]
fn edit_vs_delete_revived_by_move_no_edit_conflict() {
    // D-001 Option A: A deletes x, C (later) moves x to /b: x live via the
    // move, edit kept; the edit has nothing to revive.
    let (mut ops, _, b, x) = base();
    ops.push(op(5, A, &[x], Payload::Delete { node: x }));
    ops.push(set(5, B, &[x], x, b"E"));
    ops.push(op(
        5,
        C,
        &[x],
        Payload::Move {
            node: x,
            new_parent: b,
            new_name: "x".into(),
        },
    ));
    let ev = all_orders_agree(&ops);
    assert!(is_live(&ev.state, x));
    assert_eq!(project(&ev.state).paths[&x], "b/x");
    assert_eq!(project(&ev.state).blobs[&x], BlobId::of(b"E"));
    assert!(content_conflicts(&ev).is_empty());
}

#[test]
fn edit_causally_after_delete_revives() {
    // [decision-proposal] (D-014 leaves it to the spec): an edit that saw the
    // delete also revives, with the same conflict.
    let (mut ops, a, _, x) = base();
    let d = op(5, A, &[x], Payload::Delete { node: x });
    let e = set(6, B, &[d.id], x, b"E");
    ops.extend([d.clone(), e.clone()]);
    let ev = all_orders_agree(&ops);
    assert_eq!(ev.state[&x].parent, Some(a));
    assert_eq!(
        content_conflicts(&ev),
        BTreeSet::from([Conflict::EditRevived {
            node: x,
            edit: e.id,
            delete: d.id
        }])
    );
}

#[test]
fn delete_causally_after_edit_stays_deleted() {
    let (mut ops, _, _, x) = base();
    let e = set(5, A, &[x], x, b"E");
    let d = op(6, B, &[e.id], Payload::Delete { node: x });
    ops.extend([e, d]);
    let ev = all_orders_agree(&ops);
    assert!(!is_live(&ev.state, x));
    assert_eq!(ev.state[&x].content, Some(BlobId::of(b"E")));
    assert!(content_conflicts(&ev).is_empty());
}

#[test]
fn redelete_after_seeing_revive_sticks() {
    let (mut ops, _, _, x) = base();
    let e = set(5, A, &[x], x, b"E");
    let d = op(5, B, &[x], Payload::Delete { node: x });
    let d2 = op(6, B, &[e.id, d.id], Payload::Delete { node: x });
    ops.extend([e, d, d2]);
    let ev = all_orders_agree(&ops);
    assert!(!is_live(&ev.state, x));
    assert!(content_conflicts(&ev).is_empty());
}

#[test]
fn edit_vs_move_both_orders() {
    for (erid, mrid) in [(A, B), (B, A)] {
        let (mut ops, _, b, x) = base();
        ops.push(set(5, erid, &[x], x, b"E"));
        ops.push(op(
            5,
            mrid,
            &[x],
            Payload::Move {
                node: x,
                new_parent: b,
                new_name: "y".into(),
            },
        ));
        let ev = all_orders_agree(&ops);
        let p = project(&ev.state);
        assert_eq!(p.paths[&x], "b/y");
        assert_eq!(p.blobs[&x], BlobId::of(b"E"));
        assert!(content_conflicts(&ev).is_empty());
    }
}

#[test]
fn set_content_on_directory_root_trash_or_unknown_is_invalid() {
    let (ops0, a, _, x) = base();
    for target in [a, ROOT, TRASH, NodeId::new(99, C)] {
        let mut ops = ops0.clone();
        let bad = set(5, B, &[x], target, b"E");
        ops.push(bad.clone());
        let ev = all_orders_agree(&ops);
        assert_eq!(ev.outcomes[&bad.id], Outcome::Invalid);
        assert!(ev.conflicts.contains(&Conflict::InvalidOp { op: bad.id }));
        assert_eq!(ev.state, evaluate(&ops0).state);
        assert!(check_invariants(&ev.state, &ops, &ev.outcomes).is_ok());
    }
}

#[test]
fn generator_emits_content_scenarios() {
    // Sanity: the shared generator produces concurrent content conflicts and
    // edit-vs-delete conflicts for some scripts.
    let (mut c, mut d, mut inv) = (false, false, false);
    let mut with_revive = 0usize;
    for s in 0..400u64 {
        let mut x = s.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let script: Vec<(u8, u8, u8, u8)> = (0..40)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let b = x.to_le_bytes();
                (b[0], b[1], b[2], b[3])
            })
            .collect();
        let ops = simulate(3, &script);
        let ev = evaluate(&ops);
        if ev
            .conflicts
            .iter()
            .any(|c| matches!(c, Conflict::EditRevived { .. }))
        {
            with_revive += 1;
        }
        for cf in &ev.conflicts {
            match cf {
                Conflict::Content { .. } => c = true,
                Conflict::EditRevived { .. } => d = true,
                Conflict::InvalidOp { op }
                    if matches!(
                        ops.iter().find(|o| o.id == *op).unwrap().payload,
                        Payload::SetContent { .. }
                    ) =>
                {
                    inv = true
                }
                _ => {}
            }
        }
    }
    assert!(
        c && d && inv,
        "content={c} edit-revived={d} invalid-set={inv}"
    );
    // Measured rate (Issue #20): printed with --nocapture, floor asserted.
    println!("edit-revived histories: {with_revive}/400");
    assert!(
        with_revive >= 20,
        "edit-revived rate too low: {with_revive}/400"
    );
}
