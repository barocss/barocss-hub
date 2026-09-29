//! Issue #31: lazy checkpoint decode (`FrontierCheck`,
//! `Checkpoint::from_frontier_checked`) equals the pre-#31 eager oracle
//! (`Checkpoint::from_frontier_eager`, a down-closure per checkpoint) on the
//! checkpoint (or error), full and short ids, the labelled version, and
//! `VersionIndex` resolution, for the shared #5 histories plus random
//! frontiers that are unknown, non-antichain, or over stores that are not
//! closed or not Lamport (the eager fallback).

use std::collections::BTreeSet;

use hub_core::checkpoint::*;
use hub_core::tree::*;
use hub_core::version::*;
use proptest::prelude::*;

mod common;
use common::*;

struct Xs(u64);
impl Xs {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Store mutations: 0 = as generated (closed, Lamport); 1 = extra deps on
/// random smaller ops (still closed and Lamport); 2 = a non-Lamport dep;
/// 3 = a dep on an unknown op (not closed).
fn mutate(ops: &[Op], mode: u8, rng: &mut Xs) -> Store {
    let mut store: Store = ops.iter().map(|o| (o.id, o.clone())).collect();
    let ids: Vec<OpId> = store.keys().copied().collect();
    if ids.is_empty() {
        return store;
    }
    match mode % 4 {
        1 => {
            for _ in 0..3 {
                let i = rng.below(ids.len());
                if i > 0 {
                    let j = rng.below(i);
                    store.get_mut(&ids[i]).unwrap().deps.insert(ids[j]);
                }
            }
        }
        2 => {
            let i = rng.below(ids.len());
            let j = rng.below(ids.len());
            if ids[j] > ids[i] {
                store.get_mut(&ids[i]).unwrap().deps.insert(ids[j]);
            }
        }
        3 => {
            let i = rng.below(ids.len());
            store
                .get_mut(&ids[i])
                .unwrap()
                .deps
                .insert(OpId::new(9_999, 0xdead));
        }
        _ => {}
    }
    store
}

/// A random frontier: mostly store ids (often several, often comparable,
/// so not an antichain), sometimes an unknown id, sometimes empty.
fn frontier(store: &Store, rng: &mut Xs) -> Vec<OpId> {
    let ids: Vec<OpId> = store.keys().copied().collect();
    let n = rng.below(5);
    let mut f: Vec<OpId> = (0..n)
        .filter(|_| !ids.is_empty())
        .map(|_| ids[rng.below(ids.len())])
        .collect();
    if rng.below(6) == 0 {
        f.push(OpId::new(1 + rng.below(50) as u64, 0xbad));
    }
    f
}

fn history() -> impl Strategy<Value = (Vec<Op>, u8, u64)> {
    (script_strategy(), any::<u8>(), any::<u64>())
        .prop_map(|((nrep, script, _), mode, seed)| (simulate(nrep, &script), mode, seed))
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn lazy_decode_equals_eager_oracle((ops, mode, seed) in history()) {
        let mut rng = Xs(seed | 1);
        let store = mutate(&ops, mode, &mut rng);
        let check = FrontierCheck::new(&store);
        let mut lazy = Checkpoints::new();
        let mut eager = Checkpoints::new();
        for k in 0..8 {
            let f = frontier(&store, &mut rng);
            let msg = format!("c{k}");
            let note = (k % 3 == 0).then(|| format!("n{k}"));
            let a = Checkpoint::from_frontier_checked(&check, f.clone(), msg.clone(), ReplicaId(7), note.clone());
            let b = Checkpoint::from_frontier_eager(&store, f.clone(), msg, ReplicaId(7), note);
            prop_assert_eq!(&a, &b, "frontier {:?}", f);
            // The one-shot constructor is the lazy path too.
            let c = Checkpoint::from_frontier(&store, f.clone(), format!("c{k}"), ReplicaId(7), (k % 3 == 0).then(|| format!("n{k}")));
            prop_assert_eq!(&c, &b);
            if let (Ok(a), Ok(b)) = (a, b) {
                prop_assert_eq!(a.id(), b.id());
                prop_assert_eq!(a.id().short(), b.id().short());
                prop_assert_eq!(a.frontier(), b.frontier());
                // Memoized down-closure: same on first and second call.
                prop_assert_eq!(a.version(&store), b.version(&store));
                prop_assert_eq!(a.version(&store), b.version(&store));
                if let Ok(v) = b.version(&store) {
                    prop_assert_eq!(a.frontier().to_vec(), v.frontier(&store));
                }
                lazy.add(a);
                eager.add(b);
            }
        }
        prop_assert_eq!(&lazy, &eager);
        // Comparisons between labelled versions agree.
        let lv: Vec<_> = lazy.iter().map(|c| c.version(&store)).collect();
        let ev: Vec<_> = eager.iter().map(|c| c.version(&store)).collect();
        for i in 0..lv.len() {
            for j in 0..lv.len() {
                if let (Ok(a), Ok(b), Ok(c), Ok(d)) = (&lv[i], &lv[j], &ev[i], &ev[j]) {
                    prop_assert_eq!(compare(a, b), compare(c, d));
                }
            }
        }
        // Resolution and short display through the index (closed stores:
        // the index down-closes principal versions).
        let closed = store.values().all(|o| o.deps.iter().all(|d| store.contains_key(d)));
        if closed {
            let il = VersionIndex::with_checkpoints(&store, &lazy);
            let ie = VersionIndex::with_checkpoints(&store, &eager);
            prop_assert_eq!(il.len(), ie.len());
            for c in lazy.iter() {
                let id = c.id();
                prop_assert_eq!(il.short(&id), ie.short(&id));
                for n in [4usize, 8, 64] {
                    let p = &id.hex()[..n];
                    let rl = il.resolve(p).map(|(i, f)| (i, f.to_vec()));
                    let re = ie.resolve(p).map(|(i, f)| (i, f.to_vec()));
                    prop_assert_eq!(&rl, &re);
                    if rl.is_ok() {
                        prop_assert_eq!(il.resolve_version(&store, p), ie.resolve_version(&store, p));
                    }
                }
            }
        }
    }
}

/// Invalid stored frontiers are detected exactly as before: an unknown id
/// is `Missing` (the greatest unknown one); a non-antichain is absorbed to
/// its canonical frontier (the eager path never rejected it).
#[test]
fn invalid_frontiers_detected_as_before() {
    let a = OpId::new(2, 1);
    let b = OpId::new(3, 1);
    let c = OpId::new(3, 2);
    let mk = |id: OpId, deps: &[OpId]| Op {
        id,
        deps: deps.iter().copied().collect::<BTreeSet<_>>(),
        payload: Payload::Create {
            kind: Kind::Directory,
            parent: ROOT,
            name: format!("d{}", id.counter),
        },
    };
    let store: Store = [mk(a, &[]), mk(b, &[a]), mk(c, &[a])]
        .into_iter()
        .map(|o| (o.id, o))
        .collect();
    let check = FrontierCheck::new(&store);
    let u1 = OpId::new(9, 9);
    let u2 = OpId::new(10, 9);
    let lazy = Checkpoint::from_frontier_checked(&check, [a, u1, u2], "m", ReplicaId(1), None);
    assert_eq!(lazy, Err(VersionError::Missing(u2)));
    assert_eq!(
        lazy,
        Checkpoint::from_frontier_eager(&store, [a, u1, u2], "m", ReplicaId(1), None)
    );
    // Non-antichain {a, b, c}: a ≺ b, so canonical frontier is {b, c}.
    let l = Checkpoint::from_frontier_checked(&check, [a, b, c], "m", ReplicaId(1), None).unwrap();
    assert_eq!(l.frontier(), &[b, c]);
    assert_eq!(
        l,
        Checkpoint::from_frontier_eager(&store, [a, b, c], "m", ReplicaId(1), None).unwrap()
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// Issue #31: range-based `short` / `resolve` equal the pre-#31 scans,
    /// including forced long shared prefixes (ids that collide on 8+ hex
    /// digits are built by inserting frontiers of many ops).
    #[test]
    fn short_and_resolve_equal_scan((ops, _mode, seed) in history()) {
        let store: Store = ops.iter().map(|o| (o.id, o.clone())).collect();
        let ix = VersionIndex::new(&store);
        let mut rng = Xs(seed | 1);
        let mut probes: Vec<VersionId> = store.keys().map(|&o| VersionId::of_frontier(&[o])).collect();
        probes.push(VersionId::of_frontier(&[]));
        probes.push(VersionId([rng.next() as u8; 32]));
        for id in &probes {
            prop_assert_eq!(ix.short(id), ix.short_scan(id));
            let h = id.hex();
            for n in [1usize, 2, 3, 4, 5, 7, 8, 64] {
                let p = &h[..n];
                let hits = match ix.resolve(p) {
                    Ok((i, _)) => vec![i],
                    Err(ResolveError::Ambiguous(v)) => v,
                    Err(ResolveError::NotFound) => vec![],
                    Err(ResolveError::InvalidPrefix) => { prop_assert!(n < MIN_PREFIX); continue }
                };
                prop_assert_eq!(hits, ix.resolve_scan(p));
                prop_assert_eq!(ix.resolve(&p.to_ascii_uppercase()), ix.resolve(p));
            }
        }
    }

    /// Issue #32: the split index (principal array + explicit map) equals
    /// the pre-#32 single map on length, `short` and `resolve` (id and
    /// frontier), with checkpoints and explicitly inserted versions.
    #[test]
    fn split_index_equals_single_map((ops, _mode, seed) in history()) {
        let store: Store = ops.iter().map(|o| (o.id, o.clone())).collect();
        let mut rng = Xs(seed | 1);
        let mut cps = Checkpoints::new();
        let mut extra = Vec::new();
        for k in 0..6 {
            let f = frontier(&store, &mut rng);
            if let Ok(c) = Checkpoint::from_frontier(&store, f, format!("c{k}"), ReplicaId(1), None) {
                extra.push(c.frontier().to_vec());
                cps.add(c);
            }
        }
        let mut new = VersionIndex::with_checkpoints(&store, &cps);
        let mut old = VersionIndex::new_single_map(&store, &cps);
        for f in &extra {
            prop_assert_eq!(new.insert_frontier(f.clone()), old.insert_frontier(f.clone()));
        }
        prop_assert_eq!(new.len(), old.len());
        let mut probes: Vec<VersionId> = store.keys().map(|&o| VersionId::of_frontier(&[o])).collect();
        probes.extend(cps.iter().map(|c| c.id()));
        probes.push(VersionId::of_frontier(&[]));
        probes.push(VersionId([rng.next() as u8; 32]));
        for id in &probes {
            prop_assert_eq!(new.short(id), old.short(id));
            let h = id.hex();
            for n in [3usize, 4, 5, 8, 64] {
                let p = &h[..n];
                let a = new.resolve(p).map(|(i, f)| (i, f.to_vec()));
                let b = old.resolve(p).map(|(i, f)| (i, f.to_vec()));
                prop_assert_eq!(a, b);
            }
        }
    }
}
