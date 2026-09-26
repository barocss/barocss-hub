//! Issue #12: version model properties (Repository-Semantics §3.5 #2-#7),
//! generator check for §2.6 #7, and the §3.6 worked example.

use std::collections::BTreeSet;

use hub_core::projection::project;
use hub_core::tree::*;
use hub_core::version::*;
use proptest::prelude::*;

mod common;
use common::*;

fn store_of(ops: &[Op]) -> Store {
    ops.iter().map(|o| (o.id, o.clone())).collect()
}

/// A random version of the history: down-closure of a random id subset.
fn pick_version(store: &Store, seed: u64) -> Version {
    let mut s = seed | 1;
    let mut picks = BTreeSet::new();
    for id in store.keys() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        // ~1/4 chance, so frontiers are small but varied (and sometimes empty).
        if s % 4 == 0 {
            picks.insert(*id);
        }
    }
    Version::downclose(store, picks).expect("store ids resolve")
}

fn closed(store: &Store, v: &Version) -> bool {
    Version::from_ops(store, v.ops().clone()).is_ok()
}

fn history() -> impl Strategy<Value = (Store, [u64; 3], u64)> {
    (script_strategy(), any::<[u64; 3]>(), any::<u64>()).prop_map(
        |((nrep, script, _), seeds, dseed)| (store_of(&simulate(nrep, &script)), seeds, dseed),
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    /// §2.6 #7: the shared generator gives every replica an own causal chain.
    #[test]
    fn p2_6_7_generator_own_chain_deps((store, _, _) in history()) {
        prop_assert!(own_chain_ok(&store));
    }

    /// §3.5 #2: downclose(F(V)) = V, and F(V1) = F(V2) <=> V1 = V2.
    #[test]
    fn p3_5_2_frontier_determines_version((store, [s1, s2, _], _) in history()) {
        let v1 = pick_version(&store, s1);
        let v2 = pick_version(&store, s2);
        for v in [&v1, &v2] {
            let back = Version::downclose(&store, v.frontier(&store)).unwrap();
            prop_assert_eq!(&back, v);
        }
        prop_assert_eq!(v1.frontier(&store) == v2.frontier(&store), v1 == v2);
        prop_assert!(Version::genesis().frontier(&store).is_empty());
    }

    /// §3.5 #3: the frontier is a sorted, duplicate-free antichain.
    #[test]
    fn p3_5_3_frontier_is_antichain((store, [s1, _, _], _) in history()) {
        let f = pick_version(&store, s1).frontier(&store);
        prop_assert!(f.windows(2).all(|w| w[0] < w[1]));
        for &a in &f {
            for &b in &f {
                prop_assert!(a == b || !causally_below(&store, a, b));
            }
        }
    }

    /// §3.5 #4: ⊔/⊓ lattice laws, results causally closed.
    #[test]
    fn p3_5_4_merge_laws((store, [s1, s2, s3], _) in history()) {
        let (a, b, c) = (pick_version(&store, s1), pick_version(&store, s2), pick_version(&store, s3));
        let g = Version::genesis();
        prop_assert_eq!(a.merge(&b), b.merge(&a));
        prop_assert_eq!(a.merge(&b).merge(&c), a.merge(&b.merge(&c)));
        prop_assert_eq!(a.merge(&a), a.clone());
        prop_assert_eq!(a.merge(&g), a.clone());
        prop_assert_eq!(a.meet(&b), b.meet(&a));
        prop_assert_eq!(a.meet(&b).meet(&c), a.meet(&b.meet(&c)));
        prop_assert_eq!(a.meet(&a), a.clone());
        prop_assert_eq!(a.meet(&g), g.clone());
        prop_assert_eq!(a.merge(&a.meet(&b)), a.clone());
        prop_assert_eq!(a.meet(&a.merge(&b)), a.clone());
        prop_assert!(closed(&store, &a.merge(&b)));
        prop_assert!(closed(&store, &a.meet(&b)));
    }

    /// §3.5 #5: F(V1 ⊔ V2) = max≺(F(V1) ∪ F(V2)).
    #[test]
    fn p3_5_5_merge_frontier((store, [s1, s2, _], _) in history()) {
        let (a, b) = (pick_version(&store, s1), pick_version(&store, s2));
        prop_assert_eq!(
            a.merge(&b).frontier(&store),
            merge_frontiers(&store, &a.frontier(&store), &b.frontier(&store))
        );
    }

    /// §3.5 #6: compare agrees with inclusion; set, frontier and VV tests
    /// agree (VV relies on §2.6 #7); V1 is ancestor-or-equal of V1 ⊔ V2.
    #[test]
    fn p3_5_6_compare_consistent_with_inclusion((store, [s1, s2, _], _) in history()) {
        let (a, b) = (pick_version(&store, s1), pick_version(&store, s2));
        let expect = match (a.ops().is_subset(b.ops()), b.ops().is_subset(a.ops())) {
            (true, true) => Relation::Equal,
            (true, false) => Relation::Ancestor,
            (false, true) => Relation::Descendant,
            (false, false) => Relation::Concurrent,
        };
        prop_assert_eq!(compare(&a, &b), expect);
        prop_assert_eq!(compare_frontier(&store, &a, &b), expect);
        prop_assert_eq!(compare_vv(&a.vv(), &b.vv()), expect);
        let m = a.merge(&b);
        prop_assert!(matches!(compare(&a, &m), Relation::Ancestor | Relation::Equal));
        prop_assert!(matches!(compare(&a.meet(&b), &a), Relation::Ancestor | Relation::Equal));
    }

    /// §3.5 #7: VV(V1) = VV(V2) <=> V1 = V2, and VV round-trips to V.
    #[test]
    fn p3_5_7_vv_determines_version((store, [s1, s2, _], _) in history()) {
        let (a, b) = (pick_version(&store, s1), pick_version(&store, s2));
        prop_assert_eq!(a.vv() == b.vv(), a == b);
        prop_assert_eq!(Version::from_vv(&store, &a.vv()).unwrap(), a);
    }

    /// §3.5 #8 (asked by #12 as the "whichever replica" property): two
    /// replicas holding V1 and V2 that exchange ops in shuffled, duplicated
    /// order reach the same State, Conflicts and Projection as the spec
    /// evaluated on V1 ⊔ V2.
    #[test]
    fn p3_5_8_merge_result_independent_of_replica((store, [s1, s2, _], dseed) in history()) {
        let (a, b) = (pick_version(&store, s1), pick_version(&store, s2));
        let m = a.merge(&b);
        let spec = evaluate(m.resolve(&store));
        let spec_proj = project(&spec.state);
        let replica_with = |mine: &Version, theirs: &Version, seed: u64| {
            let mut r = Replica::new();
            let own: Vec<Op> = mine.resolve(&store).cloned().collect();
            for op in delivery(&own, seed) { r.receive(op); }
            let other: Vec<Op> = theirs.resolve(&store).cloned().collect();
            for op in delivery(&other, seed ^ 0x9e37) { r.receive(op); }
            r
        };
        for r in [replica_with(&a, &b, dseed), replica_with(&b, &a, dseed.rotate_left(17))] {
            prop_assert_eq!(r.pending_len(), 0);
            let got = Version::from_ops(&store, r.ops().map(|o| o.id).collect()).unwrap();
            prop_assert_eq!(&got, &m);
            prop_assert_eq!(got.frontier(&store), m.frontier(&store));
            prop_assert_eq!(r.state(), &spec.state);
            prop_assert_eq!(&evaluate(r.ops()).conflicts, &spec.conflicts);
            prop_assert_eq!(&project(r.state()), &spec_proj);
        }
    }
}

#[test]
fn from_ops_rejects_unclosed_and_missing() {
    let a1 = Op {
        id: NodeId::new(1, 1),
        deps: BTreeSet::new(),
        payload: Payload::Create {
            kind: Kind::Directory,
            parent: ROOT,
            name: "p".into(),
        },
    };
    let a2 = Op {
        id: NodeId::new(2, 1),
        deps: [a1.id].into(),
        payload: Payload::Delete { node: a1.id },
    };
    let store = store_of(&[a1.clone(), a2.clone()]);
    assert_eq!(
        Version::from_ops(&store, [a2.id].into()),
        Err(VersionError::NotClosed {
            op: a2.id,
            dep: a1.id
        })
    );
    let ghost = NodeId::new(9, 9);
    assert_eq!(
        Version::downclose(&store, [ghost]),
        Err(VersionError::Missing(ghost))
    );
}

#[test]
fn own_chain_violation_is_detected() {
    // Replica 1 authors (2,1) without depending on its own (1,1).
    let mk = |c, deps: &[OpId], name: &str| Op {
        id: NodeId::new(c, 1),
        deps: deps.iter().copied().collect(),
        payload: Payload::Create {
            kind: Kind::Directory,
            parent: ROOT,
            name: name.into(),
        },
    };
    let store = store_of(&[mk(1, &[], "a"), mk(2, &[], "b")]);
    assert!(!own_chain_ok(&store));
    // VV is only a hint here: {(2,1)} and {(1,1),(2,1)} share VV {1: 2}.
    let lone = Version::downclose(&store, [NodeId::new(2, 1)]).unwrap();
    let both = Version::downclose(&store, store.keys().copied()).unwrap();
    assert_ne!(lone, both);
    assert_eq!(lone.vv(), both.vv());
    assert_eq!(compare(&lone, &both), Relation::Ancestor);
    assert_eq!(compare_vv(&lone.vv(), &both.vv()), Relation::Equal);
}

/// §3.6 worked example, replicas A=1, B=2, C=3.
#[test]
fn worked_example_3_6() {
    const A: u128 = 1;
    const B: u128 = 2;
    const C: u128 = 3;
    let op = |c, r, deps: &[OpId], payload| Op {
        id: NodeId::new(c, r),
        deps: deps.iter().copied().collect(),
        payload,
    };
    let a1 = op(
        1,
        A,
        &[],
        Payload::Create {
            kind: Kind::Directory,
            parent: ROOT,
            name: "p".into(),
        },
    );
    let p = a1.id;
    let a2 = op(
        2,
        A,
        &[p],
        Payload::Move {
            node: p,
            new_parent: ROOT,
            new_name: "q".into(),
        },
    );
    let b1 = op(
        2,
        B,
        &[p],
        Payload::Create {
            kind: Kind::File,
            parent: p,
            name: "f".into(),
        },
    );
    let c1 = op(
        3,
        C,
        &[a2.id],
        Payload::Create {
            kind: Kind::File,
            parent: p,
            name: "g".into(),
        },
    );
    let mut store = store_of(&[a1.clone(), a2.clone(), b1.clone(), c1.clone()]);
    assert!(own_chain_ok(&store));

    let va = Version::from_ops(&store, [a1.id, a2.id].into()).unwrap();
    let vb = Version::from_ops(&store, [a1.id, b1.id].into()).unwrap();
    let vc = Version::from_ops(&store, [a1.id, a2.id, c1.id].into()).unwrap();
    let rid = |r| ReplicaId(r);
    let vv = |xs: &[(u128, u64)]| -> VersionVector {
        xs.iter()
            .filter(|x| x.1 > 0)
            .map(|&(r, c)| (rid(r), c))
            .collect()
    };
    let paths = |v: &Version| -> Vec<String> {
        let mut ps: Vec<String> = project(&evaluate(v.resolve(&store)).state)
            .entries
            .into_keys()
            .filter(|p| !p.is_empty())
            .collect();
        ps.sort();
        ps
    };

    // Table rows.
    assert_eq!(va.frontier(&store), vec![a2.id]);
    assert_eq!(vb.frontier(&store), vec![b1.id]);
    assert_eq!(vc.frontier(&store), vec![c1.id]);
    assert_eq!(va.vv(), vv(&[(A, 2), (B, 0), (C, 0)]));
    assert_eq!(vb.vv(), vv(&[(A, 1), (B, 2), (C, 0)]));
    assert_eq!(vc.vv(), vv(&[(A, 2), (B, 0), (C, 3)]));
    assert_eq!(paths(&va), vec!["q".to_string()]);
    assert_eq!(paths(&vb), vec!["p".to_string(), "p/f".to_string()]);
    assert_eq!(paths(&vc), vec!["q".to_string(), "q/g".to_string()]);

    // Comparisons.
    assert_eq!(compare(&va, &vc), Relation::Ancestor);
    assert_eq!(compare(&vc, &va), Relation::Descendant);
    assert_eq!(compare(&va, &vb), Relation::Concurrent);
    assert_eq!(compare(&vb, &vc), Relation::Concurrent);
    let gca = vb.meet(&vc);
    assert_eq!(gca.frontier(&store), vec![a1.id]);

    // Merge B ⊔ C.
    let m = vb.merge(&vc);
    assert_eq!(*m.ops(), BTreeSet::from([a1.id, a2.id, b1.id, c1.id]));
    assert_eq!(m.frontier(&store), vec![b1.id, c1.id]);
    assert_eq!(
        merge_frontiers(&store, &vb.frontier(&store), &vc.frontier(&store)),
        vec![b1.id, c1.id]
    );
    assert_eq!(m.vv(), vv(&[(A, 2), (B, 2), (C, 3)]));
    let ev = evaluate(m.resolve(&store));
    assert!(ev.conflicts.is_empty());
    assert_eq!(
        paths(&m),
        vec!["q".to_string(), "q/f".to_string(), "q/g".to_string()]
    );
    // Merging A in changes nothing; all three converge.
    assert_eq!(m.merge(&va), m);
    assert_eq!(
        va.merge(&vb).merge(&vc).frontier(&store),
        vec![b1.id, c1.id]
    );
    assert_eq!(compare(&va, &m), Relation::Ancestor);

    // b2 = (4,B) authored on the merged version collapses the frontier.
    let b2 = op(
        4,
        B,
        &m.frontier(&store),
        Payload::Move {
            node: b1.id,
            new_parent: p,
            new_name: "f".into(),
        },
    );
    store.insert(b2.id, b2.clone());
    let m2 = Version::downclose(&store, [b2.id]).unwrap();
    assert_eq!(m2.frontier(&store), vec![b2.id]);
    assert_eq!(compare(&m, &m2), Relation::Ancestor);
}
