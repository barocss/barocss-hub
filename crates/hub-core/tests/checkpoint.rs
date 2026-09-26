//! Issue #13: checkpoints (D-004) and short version ids (D-005), §3.4.

use std::collections::BTreeSet;

use hub_core::checkpoint::*;
use hub_core::projection::project;
use hub_core::tree::*;
use hub_core::version::*;
use proptest::prelude::*;

mod common;
use common::*;

fn store_of(ops: &[Op]) -> Store {
    ops.iter().map(|o| (o.id, o.clone())).collect()
}

fn pick_version(store: &Store, seed: u64) -> Version {
    let mut s = seed | 1;
    let mut picks = BTreeSet::new();
    for id in store.keys() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        if s % 4 == 0 {
            picks.insert(*id);
        }
    }
    Version::downclose(store, picks).expect("store ids resolve")
}

fn history() -> impl Strategy<Value = (Vec<Op>, [u64; 3], u64)> {
    (script_strategy(), any::<[u64; 3]>(), any::<u64>())
        .prop_map(|((nrep, script, _), seeds, dseed)| (simulate(nrep, &script), seeds, dseed))
}

/// Replica that received exactly the ops of `v`, in a shuffled order.
fn replica_holding(store: &Store, v: &Version, seed: u64) -> Replica {
    let ops: Vec<Op> = v.resolve(store).cloned().collect();
    let mut r = Replica::new();
    for op in delivery(&ops, seed) {
        r.receive(op);
    }
    r
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    /// Same version on two replicas (different delivery orders, each with
    /// its own store) gives the same full and short id.
    #[test]
    fn id_independent_of_replica_and_delivery((ops, [s1, _, _], dseed) in history()) {
        let store = store_of(&ops);
        let v = pick_version(&store, s1);
        let ra = replica_holding(&store, &v, dseed);
        let rb = replica_holding(&store, &v, dseed.rotate_left(29) ^ 0x5bd1);
        let sa = store_of(&ra.ops().cloned().collect::<Vec<_>>());
        let sb = store_of(&rb.ops().cloned().collect::<Vec<_>>());
        let va = Version::from_ops(&sa, sa.keys().copied().collect()).unwrap();
        let vb = Version::from_ops(&sb, sb.keys().copied().collect()).unwrap();
        let id = version_id(&store, &v);
        prop_assert_eq!(version_id(&sa, &va), id);
        prop_assert_eq!(version_id(&sb, &vb), id);
        prop_assert_eq!(version_id(&sa, &va).short(), id.short());
        prop_assert_eq!(VersionIndex::new(&sa).short(&id).len() >= SHORT_LEN, true);
    }

    /// Distinct versions have distinct full ids in the sample; every indexed
    /// version resolves from its display short id, and any shorter prefix
    /// either resolves to it or reports ambiguity listing it.
    #[test]
    fn distinct_versions_distinct_ids_and_resolution((ops, [s1, s2, s3], _) in history()) {
        let store = store_of(&ops);
        let vs: Vec<Version> = [s1, s2, s3].iter().map(|&s| pick_version(&store, s)).collect();
        for a in &vs {
            for b in &vs {
                prop_assert_eq!(version_id(&store, a) == version_id(&store, b), a == b);
            }
        }
        let mut ix = VersionIndex::new(&store);
        for v in &vs { ix.insert(&store, v); }
        for v in &vs {
            let id = version_id(&store, v);
            let short = ix.short(&id);
            prop_assert_eq!(&ix.resolve_version(&store, &short).unwrap(), v);
            prop_assert_eq!(ix.resolve(&id.hex()).unwrap().0, id);
            for n in MIN_PREFIX..short.len() {
                match ix.resolve(&short[..n]) {
                    Ok((got, _)) => prop_assert_eq!(got, id),
                    Err(ResolveError::Ambiguous(ids)) => {
                        prop_assert!(ids.len() >= 2 && ids.contains(&id));
                        prop_assert!(ids.iter().all(|i| i.hex().starts_with(&short[..n])));
                    }
                    Err(e) => prop_assert!(false, "unexpected {:?}", e),
                }
            }
        }
    }

    /// Adding checkpoints leaves State, Conflicts, Projection (and the op
    /// set) unchanged; the checkpoint frontier round-trips to the version.
    #[test]
    fn checkpoint_is_inert_and_round_trips((ops, [s1, s2, _], dseed) in history()) {
        let store = store_of(&ops);
        let r = replica_holding(&store, &Version::downclose(&store, store.keys().copied()).unwrap(), dseed);
        let before = (r.state().clone(), evaluate(r.ops()).conflicts, project(r.state()));
        let ops_before: BTreeSet<OpId> = r.ops().map(|o| o.id).collect();
        let mut cps = Checkpoints::new();
        for (i, s) in [s1, s2].into_iter().enumerate() {
            let v = pick_version(&store, s);
            let c = Checkpoint::new(&store, &v, format!("c{i}"), ReplicaId(7), None);
            prop_assert_eq!(c.frontier().to_vec(), v.frontier(&store));
            prop_assert_eq!(&c.version(&store).unwrap(), &v);
            prop_assert_eq!(c.id(), version_id(&store, &v));
            let c2 = Checkpoint::from_frontier(&store, v.ops().iter().copied(), "x", ReplicaId(8), Some("n".into())).unwrap();
            prop_assert_eq!(c2.frontier(), c.frontier());
            let ix = VersionIndex::with_checkpoints(&store, &{ let mut t = Checkpoints::new(); t.add(c.clone()); t });
            prop_assert_eq!(&ix.resolve_version(&store, &ix.short(&c.id())).unwrap(), &v);
            cps.add(c);
        }
        prop_assert_eq!(cps.len(), 2);
        let after = (r.state().clone(), evaluate(r.ops()).conflicts, project(r.state()));
        prop_assert_eq!(before, after);
        prop_assert_eq!(ops_before, r.ops().map(|o| o.id).collect::<BTreeSet<_>>());
    }
}

fn mkdir(c: u64, r: u128, deps: &[OpId], name: &str) -> Op {
    Op {
        id: NodeId::new(c, r),
        deps: deps.iter().copied().collect(),
        payload: Payload::Create {
            kind: Kind::Directory,
            parent: ROOT,
            name: name.into(),
        },
    }
}

#[test]
fn genesis_single_and_multi_op_frontiers() {
    let a1 = mkdir(1, 1, &[], "a");
    let b1 = mkdir(1, 2, &[], "b");
    let a2 = mkdir(2, 1, &[a1.id], "c");
    let store = store_of(&[a1.clone(), b1.clone(), a2.clone()]);

    let g = Version::genesis();
    let single = Version::downclose(&store, [a2.id]).unwrap();
    let multi = Version::downclose(&store, [a2.id, b1.id]).unwrap();
    assert!(g.frontier(&store).is_empty());
    assert_eq!(single.frontier(&store), vec![a2.id]);
    assert_eq!(multi.frontier(&store), vec![b1.id, a2.id]);

    // Fixed vectors pin the v0 encoding.
    let gid = version_id(&store, &g);
    assert_eq!(gid, VersionId::of_frontier(&[]));
    let ids = [gid, version_id(&store, &single), version_id(&store, &multi)];
    let set: BTreeSet<_> = ids.iter().collect();
    assert_eq!(set.len(), 3);
    for id in &ids {
        assert_eq!(id.hex().len(), 64);
        assert_eq!(id.short().len(), SHORT_LEN);
    }
    // Multi-op id depends on the set, not on how it was named.
    let same = Version::downclose(&store, [b1.id, a2.id, a1.id]).unwrap();
    assert_eq!(version_id(&store, &same), ids[2]);

    let mut ix = VersionIndex::new(&store);
    ix.insert(&store, &multi);
    assert_eq!(ix.len(), 5); // genesis + 3 principal + multi
    for (id, v) in ids.iter().zip([&g, &single, &multi]) {
        assert_eq!(&ix.resolve_version(&store, &ix.short(id)).unwrap(), v);
        assert_eq!(
            &ix.resolve_version(&store, &id.short().to_uppercase())
                .unwrap(),
            v
        );
    }
    let cp = Checkpoint::new(&store, &multi, "merge b", ReplicaId(1), None);
    assert_eq!(cp.version(&store).unwrap(), multi);
    assert_eq!(ix.resolve("xyz1"), Err(ResolveError::InvalidPrefix));
    assert_eq!(ix.resolve("abc"), Err(ResolveError::InvalidPrefix));
    let ghost = NodeId::new(9, 9);
    assert_eq!(
        Checkpoint::from_frontier(&store, [ghost], "m", ReplicaId(1), None),
        Err(VersionError::Missing(ghost))
    );
}

#[test]
fn ambiguous_prefix_is_reported_and_short_id_lengthens() {
    // Independent roots: each principal version has a single-op frontier.
    // Search until two ids share their first MIN_PREFIX digits.
    let mut ops = Vec::new();
    let mut seen = std::collections::BTreeMap::new();
    let (x, y) = 'found: {
        for r in 1..100_000u128 {
            let op = mkdir(1, r, &[], "d");
            let id = VersionId::of_frontier(&[op.id]);
            ops.push(op);
            if let Some(&prev) = seen.get(&id.hex()[..MIN_PREFIX]) {
                break 'found (prev, id);
            }
            seen.insert(id.hex()[..MIN_PREFIX].to_string(), id);
        }
        panic!("no 4-digit collision");
    };
    let store = store_of(&ops);
    let ix = VersionIndex::new(&store);
    let p = &x.hex()[..MIN_PREFIX];
    match ix.resolve(p) {
        Err(ResolveError::Ambiguous(ids)) => {
            assert!(ids.contains(&x) && ids.contains(&y));
            assert!(ids.windows(2).all(|w| w[0] < w[1]));
        }
        other => panic!("expected ambiguity, got {other:?}"),
    }
    let (sx, sy) = (ix.short(&x), ix.short(&y));
    assert_ne!(sx, sy);
    assert_eq!(ix.resolve(&sx).unwrap().0, x);
    assert_eq!(ix.resolve(&sy).unwrap().0, y);
    let absent = VersionId::of_frontier(&[NodeId::new(5, 0)]);
    assert_eq!(ix.resolve(&absent.hex()), Err(ResolveError::NotFound));
}
