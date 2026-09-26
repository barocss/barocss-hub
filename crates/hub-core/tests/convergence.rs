//! Issue #5: randomized convergence + §2.7 worked scenarios.

use std::collections::{BTreeMap, BTreeSet};

use hub_core::tree::*;
use proptest::prelude::*;

mod common;
use common::*;

fn no_silent_drop(ops: &[Op], ev: &Evaluation) -> Result<(), String> {
    let map: BTreeMap<OpId, Op> = ops.iter().map(|o| (o.id, o.clone())).collect();
    let past = causal_past(&map);
    for op in ops {
        let Some(n) = op.moved_node() else { continue };
        if ev.outcomes[&op.id] == Outcome::Invalid {
            continue; // recorded as invalid-op
        }
        let takes_effect = ops
            .iter()
            .filter(|o| o.moved_node() == Some(n) && ev.outcomes[&o.id] == Outcome::Applied)
            .map(|o| o.id)
            .max()
            == Some(op.id);
        let overwritten_causally = ops.iter().any(|o| {
            o.moved_node() == Some(n)
                && ev.outcomes[&o.id] == Outcome::Applied
                && past[&o.id].contains(&op.id)
        });
        let losses = ev
            .conflicts
            .iter()
            .filter(|c| {
                matches!(c, Conflict::Cycle { loser, .. } | Conflict::SameNodeMove { loser, .. } if *loser == op.id)
            })
            .count();
        // A skipped cycle move always carries exactly its `cycle` conflict,
        // even if a causally later move of the node exists (Issue #5 choice).
        let skipped = matches!(ev.outcomes[&op.id], Outcome::Skipped { .. });
        let ok = if skipped {
            losses == 1
        } else if takes_effect || overwritten_causally {
            losses == 0
        } else {
            losses == 1
        };
        if !ok {
            return Err(format!(
                "op {:?}: effect={takes_effect} causal={overwritten_causally} losses={losses}",
                op.id
            ));
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    #[test]
    fn shuffled_duplicated_delivery_converges((nrep, script, seeds) in script_strategy()) {
        let ops = simulate(nrep, &script);
        let spec = evaluate(&ops);

        // §2.6 #1 unique ids, #2 Lamport consistency.
        let ids: BTreeSet<OpId> = ops.iter().map(|o| o.id).collect();
        prop_assert_eq!(ids.len(), ops.len());
        for o in &ops {
            for d in &o.deps {
                prop_assert!(*d < o.id);
            }
        }

        // §2.6 #4: invariants after every prefix of the sorted op list.
        let mut sorted = ops.clone();
        sorted.sort_by_key(|o| o.id);
        for i in 0..=sorted.len() {
            let ev = evaluate(&sorted[..i]);
            prop_assert!(check_invariants(&ev.state, &sorted[..i], &ev.outcomes).is_ok(),
                "prefix {}: {:?}", i, check_invariants(&ev.state, &sorted[..i], &ev.outcomes));
        }

        // §2.6 #3/#6: incremental replicas under shuffled+duplicated delivery.
        let mut finals = Vec::new();
        for &seed in &seeds {
            let mut rep = Replica::new();
            for op in delivery(&ops, seed) {
                rep.receive(op);
                let known: Vec<Op> = rep.ops().cloned().collect();
                let ev = evaluate(&known);
                prop_assert_eq!(rep.state(), &ev.state);
                prop_assert!(check_invariants(rep.state(), &known, &ev.outcomes).is_ok());
            }
            prop_assert_eq!(rep.pending_len(), 0);
            prop_assert_eq!(rep.state(), &spec.state);
            let ev = evaluate(rep.ops());
            prop_assert_eq!(&ev.conflicts, &spec.conflicts);
            finals.push(rep.state().clone());
        }
        prop_assert!(finals.windows(2).all(|w| w[0] == w[1]));

        // §2.6 #5 no silent drop.
        prop_assert!(no_silent_drop(&ops, &spec).is_ok(), "{:?}", no_silent_drop(&ops, &spec));
    }
}

// ---------- §2.7 worked scenarios ----------

const A: u128 = 1;
const B: u128 = 2;
const C: u128 = 3;

/// Initial tree `/a/`, `/b/`, `/a/x` authored by A at counters 2..4.
fn base() -> (Vec<Op>, NodeId, NodeId, NodeId) {
    let a = NodeId::new(2, A);
    let b = NodeId::new(3, A);
    let x = NodeId::new(4, A);
    let mk = |id: NodeId, deps: &[NodeId], kind, parent, name: &str| Op {
        id,
        deps: deps.iter().copied().collect(),
        payload: Payload::Create {
            kind,
            parent,
            name: name.into(),
        },
    };
    let ops = vec![
        mk(a, &[], Kind::Directory, ROOT, "a"),
        mk(b, &[a], Kind::Directory, ROOT, "b"),
        mk(x, &[b], Kind::File, a, "x"),
    ];
    (ops, a, b, x)
}

fn op(counter: u64, rid: u128, deps: &[NodeId], payload: Payload) -> Op {
    Op {
        id: NodeId::new(counter, rid),
        deps: deps.iter().copied().collect(),
        payload,
    }
}

fn mv(node: NodeId, new_parent: NodeId, name: &str) -> Payload {
    Payload::Move {
        node,
        new_parent,
        new_name: name.into(),
    }
}

fn permutations(v: &[Op]) -> Vec<Vec<Op>> {
    if v.len() <= 1 {
        return vec![v.to_vec()];
    }
    let mut out = Vec::new();
    for i in 0..v.len() {
        let mut rest = v.to_vec();
        let h = rest.remove(i);
        for mut p in permutations(&rest) {
            p.insert(0, h.clone());
            out.push(p);
        }
    }
    out
}

/// Deliver base then `extra` in every order (plus duplicates); all replicas
/// must equal the spec. Returns the spec evaluation.
fn run_all_orders(extra: &[Op]) -> Evaluation {
    let (base, ..) = base();
    let mut all = base.clone();
    all.extend_from_slice(extra);
    let spec = evaluate(&all);
    for p in permutations(extra) {
        let mut rep = Replica::new();
        for o in base.iter().chain(p.iter()).chain(p.iter()) {
            rep.receive(o.clone());
        }
        assert_eq!(rep.state(), &spec.state);
        assert_eq!(evaluate(rep.ops()).conflicts, spec.conflicts);
    }
    assert!(check_invariants(&spec.state, &all, &spec.outcomes).is_ok());
    spec
}

fn parent(ev: &Evaluation, n: NodeId) -> NodeId {
    ev.state[&n].parent.unwrap()
}

#[test]
fn row1_concurrent_cycle() {
    let (_, a, b, x) = base();
    let oa = op(5, A, &[x], mv(a, b, "a"));
    let ob = op(5, B, &[x], mv(b, a, "b"));
    let ev = run_all_orders(&[oa.clone(), ob.clone()]);
    assert_eq!(parent(&ev, a), b);
    assert_eq!(parent(&ev, b), ROOT);
    assert_eq!(
        ev.conflicts,
        [Conflict::Cycle {
            winner: oa.id,
            loser: ob.id,
            node: b
        }]
        .into()
    );
}

#[test]
fn row2_same_node_move() {
    let (_, a, b, x) = base();
    let _ = a;
    let oa = op(5, A, &[x], mv(x, b, "x"));
    let ob = op(5, B, &[x], mv(x, ROOT, "x"));
    let ev = run_all_orders(&[oa.clone(), ob.clone()]);
    assert_eq!(parent(&ev, x), ROOT);
    assert_eq!(
        ev.conflicts,
        [Conflict::SameNodeMove {
            winner: ob.id,
            loser: oa.id,
            node: x
        }]
        .into()
    );
}

#[test]
fn row5_delete_vs_move_descendant_revived_both_orders() {
    let (_, a, b, x) = base();
    let d = op(5, A, &[x], Payload::Delete { node: a });
    let m = op(5, B, &[x], mv(x, b, "x"));
    let expected: BTreeSet<Conflict> = [Conflict::Revived {
        node: x,
        delete: d.id,
        mv: m.id,
    }]
    .into();
    let ev = run_all_orders(&[d.clone(), m.clone()]);
    assert_eq!(parent(&ev, a), TRASH);
    assert_eq!(parent(&ev, x), b);
    assert!(is_live(&ev.state, x));
    assert_eq!(ev.conflicts, expected);
    // Also: B's op < A's op (swap replica roles) gives the same kind of result.
    let d2 = op(5, B, &[x], Payload::Delete { node: a });
    let m2 = op(5, A, &[x], mv(x, b, "x"));
    let ev2 = run_all_orders(&[d2.clone(), m2.clone()]);
    assert!(is_live(&ev2.state, x));
    assert_eq!(
        ev2.conflicts,
        [Conflict::Revived {
            node: x,
            delete: d2.id,
            mv: m2.id
        }]
        .into()
    );
}

#[test]
fn row6_delete_vs_move_same_node_option_a() {
    let (_, a, b, x) = base();
    let d = op(5, A, &[x], Payload::Delete { node: a });
    let m = op(5, B, &[x], mv(a, b, "a"));
    let ev = run_all_orders(&[d.clone(), m.clone()]);
    assert_eq!(parent(&ev, a), b);
    assert!(is_live(&ev.state, x));
    assert_eq!(
        ev.conflicts,
        [Conflict::SameNodeMove {
            winner: m.id,
            loser: d.id,
            node: a
        }]
        .into()
    );
    // Delete later in `<`: delete sticks, move is the loser.
    let m2 = op(5, A, &[x], mv(a, b, "a"));
    let d2 = op(5, B, &[x], Payload::Delete { node: a });
    let ev2 = run_all_orders(&[m2.clone(), d2.clone()]);
    assert_eq!(parent(&ev2, a), TRASH);
    assert!(!is_live(&ev2.state, x));
}

#[test]
fn row7_move_into_concurrently_deleted_dir() {
    let (_, a, b, x) = base();
    let oa = op(5, A, &[x], mv(a, b, "a"));
    let ob = op(5, B, &[x], Payload::Delete { node: b });
    let ev = run_all_orders(&[oa, ob]);
    assert_eq!(parent(&ev, a), b);
    assert_eq!(parent(&ev, b), TRASH);
    assert!(!is_live(&ev.state, x));
    assert!(ev.conflicts.is_empty());
}

#[test]
fn row9_explicit_restore() {
    let (_, a, _, x) = base();
    let d = op(5, A, &[x], Payload::Delete { node: a });
    let r = op(6, B, &[d.id], mv(a, ROOT, "a"));
    let ev = run_all_orders(&[d, r]);
    assert_eq!(parent(&ev, a), ROOT);
    assert!(is_live(&ev.state, x));
    assert!(ev.conflicts.is_empty());
}

#[test]
fn row10_three_way_concurrent_move_every_order() {
    let (_, _, b, x) = base();
    let oa = op(5, A, &[x], mv(x, b, "x"));
    let ob = op(5, B, &[x], mv(x, ROOT, "x"));
    let oc = op(5, C, &[x], mv(x, b, "z"));
    let ev = run_all_orders(&[oa.clone(), ob.clone(), oc.clone()]);
    assert_eq!(parent(&ev, x), b);
    assert_eq!(ev.state[&x].name, "z");
    assert_eq!(
        ev.conflicts,
        [
            Conflict::SameNodeMove {
                winner: oc.id,
                loser: oa.id,
                node: x
            },
            Conflict::SameNodeMove {
                winner: oc.id,
                loser: ob.id,
                node: x
            },
        ]
        .into()
    );
}

#[test]
fn row11_move_into_trashed_dir_causally_after() {
    let (_, a, b, x) = base();
    let d = op(5, A, &[x], Payload::Delete { node: b });
    let m = op(6, B, &[d.id], mv(x, b, "x"));
    let ev = run_all_orders(&[d, m]);
    assert_eq!(parent(&ev, b), TRASH);
    assert_eq!(parent(&ev, x), b);
    assert!(!is_live(&ev.state, x));
    assert!(is_live(&ev.state, a));
    assert!(ev.conflicts.is_empty());
}

#[test]
fn invalid_op_is_noop_and_recorded() {
    let (_, a, _, x) = base();
    let bad = op(5, B, &[x], mv(a, x, "a")); // File parent
    let ev = run_all_orders(std::slice::from_ref(&bad));
    assert_eq!(parent(&ev, a), ROOT);
    assert_eq!(ev.conflicts, [Conflict::InvalidOp { op: bad.id }].into());
}

// ---------- negative `revived` tests (Issue #5 review repair) ----------

fn has_revived(ev: &Evaluation) -> bool {
    ev.conflicts
        .iter()
        .any(|c| matches!(c, Conflict::Revived { .. }))
}

#[test]
fn revived_not_for_causal_restore() {
    // M causally after D: explicit restore of x out of the trash.
    let (_, a, b, x) = base();
    let d = op(5, A, &[x], Payload::Delete { node: a });
    let m = op(6, B, &[d.id], mv(x, b, "x"));
    let ev = run_all_orders(&[d, m]);
    assert_eq!(parent(&ev, a), TRASH);
    assert_eq!(parent(&ev, x), b);
    assert!(is_live(&ev.state, x));
    assert!(!has_revived(&ev));
    assert!(ev.conflicts.is_empty());
}

#[test]
fn revived_not_when_delete_is_overridden() {
    // d's winning op is B's move, not A's delete.
    let (_, a, b, x) = base();
    let d = op(5, A, &[x], Payload::Delete { node: a });
    let ma = op(5, B, &[x], mv(a, b, "a"));
    let mx = op(5, C, &[x], mv(x, b, "x"));
    let ev = run_all_orders(&[d.clone(), ma.clone(), mx]);
    assert_eq!(parent(&ev, a), b);
    assert_eq!(parent(&ev, x), b);
    assert!(is_live(&ev.state, x));
    assert!(!has_revived(&ev));
    assert_eq!(
        ev.conflicts,
        [Conflict::SameNodeMove {
            winner: ma.id,
            loser: d.id,
            node: a
        }]
        .into()
    );
}

#[test]
fn revived_not_when_x_stays_in_trash() {
    // x moves to /a/y/x, still inside the trashed subtree.
    let (_, a, _, x) = base();
    let y = op(
        5,
        A,
        &[x],
        Payload::Create {
            kind: Kind::Directory,
            parent: a,
            name: "y".into(),
        },
    );
    let d = op(6, A, &[y.id], Payload::Delete { node: a });
    let m = op(6, B, &[y.id], mv(x, y.id, "x"));
    let ev = run_all_orders(&[y.clone(), d, m]);
    assert_eq!(parent(&ev, a), TRASH);
    assert_eq!(parent(&ev, x), y.id);
    assert!(!is_live(&ev.state, x));
    assert!(!has_revived(&ev));
    assert!(ev.conflicts.is_empty());
}

// ---------- generator coverage floor ----------

/// Deterministic sample of the proptest strategy; guards against the
/// generator silently stopping to exercise the rare `cycle`/`revived` rules.
#[test]
fn generator_covers_cycle_and_revived() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
    let mut runner = TestRunner::new_with_rng(
        Config::default(),
        TestRng::from_seed(RngAlgorithm::ChaCha, &[7; 32]),
    );
    let strat = script_strategy();
    let (mut cyc, mut rev) = (0, 0);
    let n = 1024;
    for _ in 0..n {
        let (nrep, script, _) = strat.new_tree(&mut runner).unwrap().current();
        let ev = evaluate(&simulate(nrep, &script));
        cyc += ev
            .conflicts
            .iter()
            .any(|c| matches!(c, Conflict::Cycle { .. })) as usize;
        rev += ev
            .conflicts
            .iter()
            .any(|c| matches!(c, Conflict::Revived { .. })) as usize;
    }
    eprintln!("coverage over {n} cases: cycle={cyc} revived={rev}");
    assert!(cyc * 25 >= n, "cycle coverage {cyc}/{n} below 4%");
    assert!(rev * 50 >= n, "revived coverage {rev}/{n} below 2%");
}
