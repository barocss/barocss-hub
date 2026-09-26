//! Issue #9: filesystem projection (§1.5) determinism and injectivity.

use std::collections::BTreeSet;

use hub_core::projection::*;
use hub_core::tree::*;
use proptest::prelude::*;

mod common;
use common::*;

/// Structural checks of one projection against its state.
fn check_projection(state: &State, p: &Projection) -> Result<(), String> {
    // Injectivity: distinct live nodes -> distinct paths.
    if p.entries.len() != p.paths.len() {
        return Err(format!(
            "not injective: {} nodes, {} paths",
            p.paths.len(),
            p.entries.len()
        ));
    }
    for (n, path) in &p.paths {
        if p.entries.get(path) != Some(n) {
            return Err(format!("inverse mismatch at {path:?}"));
        }
    }
    // Exactly the live nodes have paths; trashed ones do not.
    for id in state.keys() {
        let live = is_live(state, *id);
        if live != p.paths.contains_key(id) {
            return Err(format!("{id:?} live={live} but path presence differs"));
        }
    }
    // path(n) = path(parent) / projected name; projected name = name or
    // name~suffix; suffixed iff a collision is recorded (exactly one).
    for (n, path) in &p.paths {
        if *n == ROOT {
            if !path.is_empty() {
                return Err("root path not empty".into());
            }
            continue;
        }
        let node = &state[n];
        let pp = &p.paths[&node.parent.unwrap()];
        let pname = if pp.is_empty() {
            path.as_str()
        } else {
            path.strip_prefix(&format!("{pp}/"))
                .ok_or(format!("{path:?} not under {pp:?}"))?
        };
        if pname.contains('/') {
            return Err(format!("component {pname:?} contains /"));
        }
        let cs: Vec<_> = p.collisions.iter().filter(|c| c.node == *n).collect();
        if pname == node.name {
            if !cs.is_empty() {
                return Err(format!("{n:?} plain but has collision"));
            }
        } else {
            if !pname.starts_with(&format!("{}~", node.name)) {
                return Err(format!("{pname:?} is not a suffix of {:?}", node.name));
            }
            if cs.len() != 1 || cs[0].projected_name != pname {
                return Err(format!("{n:?} suffixed with {} collisions", cs.len()));
            }
            let w = cs[0].winner;
            if !(w < *n && state[&w].name == node.name && p.paths[&w].ends_with(&node.name)) {
                return Err(format!("bad winner for {n:?}"));
            }
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    #[test]
    fn projection_deterministic_and_injective((nrep, script, seeds) in script_strategy()) {
        let ops = simulate(nrep, &script);
        let spec = evaluate(&ops);
        let expected = project(&spec.state);
        prop_assert!(check_projection(&spec.state, &expected).is_ok(),
            "{:?}", check_projection(&spec.state, &expected));
        for &seed in &seeds {
            let mut rep = Replica::new();
            for op in delivery(&ops, seed) {
                rep.receive(op);
                // Intermediate states too.
                let p = project(rep.state());
                prop_assert!(check_projection(rep.state(), &p).is_ok(),
                    "{:?}", check_projection(rep.state(), &p));
            }
            prop_assert_eq!(project(rep.state()), expected.clone());
        }
    }
}

// ---------- §2.7 row 4 ----------

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

fn create(kind: Kind, parent: NodeId, name: &str) -> Payload {
    Payload::Create {
        kind,
        parent,
        name: name.into(),
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

/// Base `/a/` (A, counter 2). Every delivery order of `extra` (duplicated)
/// must give the same projection; returns it.
fn project_all_orders(base: &[Op], extra: &[Op]) -> Projection {
    let all: Vec<Op> = base.iter().chain(extra).cloned().collect();
    let expected = project(&evaluate(&all).state);
    check_projection(&evaluate(&all).state, &expected).unwrap();
    for p in permutations(extra) {
        let mut rep = Replica::new();
        for o in base.iter().chain(p.iter()).chain(p.iter()) {
            rep.receive(o.clone());
        }
        assert_eq!(project(rep.state()), expected);
    }
    expected
}

fn base() -> (Vec<Op>, NodeId) {
    let a = op(2, A, &[], create(Kind::Directory, ROOT, "a"));
    let id = a.id;
    (vec![a], id)
}

#[test]
fn row4_concurrent_create_same_name_all_orders() {
    let (base, a) = base();
    let na = op(3, A, &[a], create(Kind::File, a, "n"));
    let nb = op(3, B, &[a], create(Kind::File, a, "n"));
    let p = project_all_orders(&base, &[nb.clone(), na.clone()]);
    assert_eq!(p.paths[&na.id], "a/n");
    assert_eq!(p.paths[&nb.id], "a/n~3-00000002");
    assert_eq!(
        p.collisions,
        [NameCollision {
            node: nb.id,
            parent: a,
            name: "n".into(),
            winner: na.id,
            projected_name: "n~3-00000002".into(),
        }]
        .into()
    );
}

#[test]
fn row4_three_way_one_collision_per_suffixed_node() {
    let (base, a) = base();
    let na = op(3, A, &[a], create(Kind::Directory, a, "n"));
    let nb = op(3, B, &[a], create(Kind::File, a, "n"));
    let nc = op(3, C, &[a], create(Kind::File, a, "n"));
    let p = project_all_orders(&base, &[na.clone(), nb.clone(), nc.clone()]);
    assert_eq!(p.paths[&na.id], "a/n");
    assert_eq!(p.paths[&nb.id], "a/n~3-00000002");
    assert_eq!(p.paths[&nc.id], "a/n~3-00000003");
    let suffixed: BTreeSet<NodeId> = p.collisions.iter().map(|c| c.node).collect();
    assert_eq!(suffixed, [nb.id, nc.id].into());
    assert_eq!(p.collisions.len(), 2);
    assert!(p.collisions.iter().all(|c| c.winner == na.id));
}

#[test]
fn row4_suffix_stable_under_unrelated_op() {
    let (base, a) = base();
    let na = op(3, A, &[a], create(Kind::File, a, "n"));
    let nb = op(3, B, &[a], create(Kind::File, a, "n"));
    let before = project_all_orders(&base, &[na.clone(), nb.clone()]);
    // Unrelated ops, concurrent with and larger/smaller than the creates:
    // a new dir at root, a rename of `a` (moves the whole group), and
    // a sibling with a different name.
    for third in [
        op(3, C, &[a], create(Kind::Directory, ROOT, "q")),
        op(1, C, &[], create(Kind::Directory, ROOT, "q")),
        op(3, C, &[a], create(Kind::File, a, "m")),
    ] {
        let after = project_all_orders(&base, &[na.clone(), nb.clone(), third]);
        assert_eq!(after.paths[&na.id], before.paths[&na.id]);
        assert_eq!(after.paths[&nb.id], before.paths[&nb.id]);
        assert_eq!(after.collisions, before.collisions);
    }
    // Renaming the parent changes paths but not the suffix or the conflict.
    let ren = op(
        4,
        C,
        &[na.id, nb.id],
        Payload::Move {
            node: a,
            new_parent: ROOT,
            new_name: "z".into(),
        },
    );
    let after = project_all_orders(&base, &[na.clone(), nb.clone(), ren]);
    assert_eq!(after.paths[&na.id], "z/n");
    assert_eq!(after.paths[&nb.id], "z/n~3-00000002");
    assert_eq!(after.collisions, before.collisions);
}

#[test]
fn suffix_never_collides_with_user_name() {
    let (base, a) = base();
    let na = op(3, A, &[a], create(Kind::File, a, "n"));
    let nb = op(3, B, &[a], create(Kind::File, a, "n"));
    // A user file literally named like B's short suffix.
    let lookalike = op(3, C, &[a], create(Kind::File, a, "n~3-00000002"));
    let p = project_all_orders(&base, &[na.clone(), nb.clone(), lookalike.clone()]);
    assert_eq!(p.paths[&lookalike.id], "a/n~3-00000002");
    assert_eq!(p.paths[&nb.id], "a/n~3-00000000000000000000000000000002");
    assert_eq!(p.entries.len(), p.paths.len());
}

#[test]
fn trashed_nodes_not_projected() {
    let (base, a) = base();
    let x = op(3, A, &[a], create(Kind::File, a, "x"));
    let d = op(4, A, &[x.id], Payload::Delete { node: a });
    let p = project_all_orders(&base, &[x.clone(), d]);
    assert!(!p.paths.contains_key(&a));
    assert!(!p.paths.contains_key(&x.id));
    assert!(!p.paths.contains_key(&TRASH));
    assert_eq!(p.paths.len(), 1); // ROOT only
}

// ---------- generator coverage floor ----------

#[test]
fn generator_covers_name_collisions() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
    let mut runner = TestRunner::new_with_rng(
        Config::default(),
        TestRng::from_seed(RngAlgorithm::ChaCha, &[9; 32]),
    );
    let strat = script_strategy();
    let (mut any, mut multi, mut total) = (0usize, 0usize, 0usize);
    let n = 1024;
    for _ in 0..n {
        let (nrep, script, _) = strat.new_tree(&mut runner).unwrap().current();
        let p = project(&evaluate(&simulate(nrep, &script)).state);
        let c = p.collisions.len();
        total += c;
        any += (c > 0) as usize;
        multi += (c > 1) as usize;
    }
    eprintln!("collision coverage over {n} cases: any={any} multi={multi} total={total}");
    assert!(any * 4 >= n, "collision coverage {any}/{n} below 25%");
}

// ---------- Issue #10: review follow-ups ----------

/// Evaluation over every delivery order of `extra` (duplicated) agrees with
/// the spec fold on state and conflicts; returns the spec evaluation.
fn eval_all_orders(base: &[Op], extra: &[Op]) -> Evaluation {
    let all: Vec<Op> = base.iter().chain(extra).cloned().collect();
    let spec = evaluate(&all);
    for p in permutations(extra) {
        let mut rep = Replica::new();
        for o in base.iter().chain(p.iter()).chain(p.iter()) {
            rep.receive(o.clone());
        }
        assert_eq!(rep.state(), &spec.state);
    }
    spec
}

/// Item 4: Create directly under TRASH, and under a trashed directory, is
/// valid (node recorded) but not live: no path, no conflict.
#[test]
fn issue10_create_under_trash_and_trashed_dir() {
    let (base, a) = base();
    let t = op(3, A, &[a], create(Kind::File, TRASH, "t"));
    let d = op(4, A, &[t.id], Payload::Delete { node: a });
    let x = op(5, A, &[d.id], create(Kind::File, a, "x"));
    let extra = [t.clone(), d, x.clone()];
    let all: Vec<Op> = base.iter().chain(&extra).cloned().collect();
    let spec = evaluate(&all);
    assert!(spec.conflicts.is_empty(), "{:?}", spec.conflicts);
    for n in [t.id, x.id] {
        assert!(spec.state.contains_key(&n), "{n:?} not created");
        assert!(!is_live(&spec.state, n));
    }
    assert_eq!(spec.state[&t.id].parent, Some(TRASH));
    assert_eq!(spec.state[&x.id].parent, Some(a));
    let p = project_all_orders(&base, &extra);
    assert!(!p.paths.contains_key(&t.id));
    assert!(!p.paths.contains_key(&x.id));
    assert!(p.collisions.is_empty());
    assert_eq!(p.paths.len(), 1); // ROOT only
}

/// Item 5: Create inside `a` concurrent with Delete of `a`, with the Create
/// ordered both before and after the Delete in the total order, in every
/// delivery order. The Create is valid either way (a trashed directory is
/// still a directory, §2.2) so the node ends up recorded, not live, without
/// a conflict.
#[test]
fn issue10_create_under_concurrently_deleted_dir_all_orders() {
    let (base, a) = base();
    for (cc, dc) in [(3, 4), (4, 3)] {
        let x = op(cc, B, &[a], create(Kind::File, a, "x"));
        let d = op(dc, A, &[a], Payload::Delete { node: a });
        let spec = eval_all_orders(&base, &[x.clone(), d.clone()]);
        assert!(spec.conflicts.is_empty(), "{:?}", spec.conflicts);
        assert_eq!(spec.outcomes[&x.id], Outcome::Applied);
        assert_eq!(spec.outcomes[&d.id], Outcome::Applied);
        assert_eq!(spec.state[&x.id].parent, Some(a));
        assert_eq!(spec.state[&a].parent, Some(TRASH));
        assert!(!is_live(&spec.state, x.id));
        let p = project_all_orders(&base, &[x.clone(), d]);
        assert_eq!(p.paths.len(), 1); // ROOT only
        assert!(p.collisions.is_empty());
    }
}

/// Item 6: two collision losers with the same short id (same counter, same
/// low 32 replica bits) must get distinct paths through the fallback chain.
/// Catches "assigned candidate not added to `used`".
#[test]
fn issue10_losers_sharing_short_id_get_distinct_paths() {
    let (base, a) = base();
    let r1: u128 = (1 << 32) | 2;
    let r2: u128 = (2 << 32) | 2;
    let w = op(3, A, &[a], create(Kind::File, a, "n"));
    let l1 = op(3, r1, &[a], create(Kind::File, a, "n"));
    let l2 = op(3, r2, &[a], create(Kind::File, a, "n"));
    assert_eq!(short_id(l1.id), short_id(l2.id));
    let p = project_all_orders(&base, &[w.clone(), l1.clone(), l2.clone()]);
    assert_eq!(p.paths[&w.id], "a/n");
    assert_eq!(p.paths[&l1.id], "a/n~3-00000002");
    assert_eq!(p.paths[&l2.id], "a/n~3-00000000000000000000000200000002");
    assert_eq!(p.entries.len(), p.paths.len());
    assert_eq!(p.collisions.len(), 2);

    // Deeper: a user name occupies l2's long form, forcing the `.1` step.
    let u = op(
        3,
        C,
        &[a],
        create(Kind::File, a, "n~3-00000000000000000000000200000002"),
    );
    let p = project_all_orders(&base, &[w, l1.clone(), l2.clone(), u.clone()]);
    assert_eq!(p.paths[&u.id], "a/n~3-00000000000000000000000200000002");
    assert_eq!(p.paths[&l1.id], "a/n~3-00000002");
    assert_eq!(p.paths[&l2.id], "a/n~3-00000000000000000000000200000002.1");
    assert_eq!(p.entries.len(), p.paths.len());
}

/// Item 7 (D-002, suffix instability accepted): a sibling created *later*
/// with the exact name `n~<short-id>` takes it; the suffixed loser falls back
/// to the long form. User-chosen names always win and are not rejected.
#[test]
fn issue10_later_sibling_takes_suffixed_name() {
    let (base, a) = base();
    let na = op(3, A, &[a], create(Kind::File, a, "n"));
    let nb = op(3, B, &[a], create(Kind::File, a, "n"));
    let before = project_all_orders(&base, &[na.clone(), nb.clone()]);
    assert_eq!(before.paths[&nb.id], "a/n~3-00000002");
    // Causally after both creates (it "sees" the suffixed path).
    let later = op(4, A, &[na.id, nb.id], create(Kind::File, a, "n~3-00000002"));
    let spec = evaluate(
        &base
            .iter()
            .chain([&na, &nb, &later])
            .cloned()
            .collect::<Vec<_>>(),
    );
    assert!(spec.conflicts.is_empty());
    let p = project_all_orders(&base, &[na.clone(), nb.clone(), later.clone()]);
    assert_eq!(p.paths[&later.id], "a/n~3-00000002");
    assert_eq!(p.paths[&na.id], "a/n");
    assert_eq!(p.paths[&nb.id], "a/n~3-00000000000000000000000000000002");
    assert_eq!(p.entries.len(), p.paths.len());
    // The user-named node is plain (no collision); nb keeps exactly one.
    assert!(p.collisions.iter().all(|c| c.node != later.id));
    let c: Vec<_> = p.collisions.iter().filter(|c| c.node == nb.id).collect();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].projected_name, "n~3-00000000000000000000000000000002");
}
