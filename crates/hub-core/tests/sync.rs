//! Issue #21: §4 sync core. Property tests for §4.7 #1-#10 on 2-4 replicas
//! with interrupted, shuffled and duplicated schedules, plus explicit tests
//! (§4.8 worked example, equivocation, Lamport violation, pending, blobs).

use std::collections::BTreeSet;

use hub_core::checkpoint::Checkpoint;
use hub_core::sync::*;
use hub_core::tree::*;
use hub_core::version::{merge_frontiers, Store, Version, VersionVector};
use proptest::prelude::*;

// ---------- helpers ----------

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn closed(store: &Store) -> bool {
    store
        .values()
        .all(|o| o.deps.iter().all(|d| store.contains_key(d)))
}

fn ids(r: &SyncReplica) -> BTreeSet<OpId> {
    r.store().keys().copied().collect()
}

fn union_store(reps: &[SyncReplica]) -> Store {
    let mut s = Store::new();
    for r in reps {
        for (k, v) in r.store() {
            s.insert(*k, v.clone());
        }
    }
    s
}

#[derive(Clone, Copy, Debug)]
enum Hint {
    None,
    True,
    Stale,
    TooHigh,
    Random,
}

fn hint_of(mode: u8) -> Hint {
    match mode % 5 {
        0 => Hint::None,
        1 => Hint::True,
        2 => Hint::Stale,
        3 => Hint::TooHigh,
        _ => Hint::Random,
    }
}

fn make_have(
    a: &SyncReplica,
    hint: Hint,
    stale: &VersionVector,
    all: &Store,
    rng: &mut Lcg,
) -> Have {
    let vv_hint = match hint {
        Hint::None => None,
        Hint::True => Some(a.vv()),
        Hint::Stale => Some(stale.clone()),
        Hint::TooHigh => Some(
            Version::from_ops(all, all.keys().copied().collect())
                .unwrap()
                .vv(),
        ),
        Hint::Random => Some(
            (1..=4u128)
                .map(|r| (ReplicaId(r), rng.next() % 40))
                .collect(),
        ),
    };
    Have {
        frontier: a.frontier(),
        vv_hint,
    }
}

/// Author a random op on replica `r` from its current state.
fn author_random(rep: &mut SyncReplica, act: u8, a: u8, b: u8, invalid: &mut BTreeSet<OpId>) {
    let st = rep.state();
    let nodes: Vec<NodeId> = st.keys().copied().filter(|n| !n.is_reserved()).collect();
    let dirs: Vec<NodeId> = st
        .values()
        .filter(|n| n.kind != Kind::File)
        .map(|n| n.id)
        .collect();
    let files: Vec<NodeId> = st
        .values()
        .filter(|n| n.kind == Kind::File)
        .map(|n| n.id)
        .collect();
    let pick = |v: &Vec<NodeId>, i: u8| v[i as usize % v.len()];
    let name = ["n", "m", "k"][b as usize % 3].to_string();
    let payload = match act % 8 {
        0 | 1 => Payload::Create {
            kind: if b % 3 == 0 {
                Kind::File
            } else {
                Kind::Directory
            },
            parent: pick(&dirs, a),
            name,
        },
        2 if !nodes.is_empty() => {
            let node = pick(&nodes, a);
            let new_parent = pick(&dirs, b);
            if is_ancestor_or_self(&st, node, new_parent) {
                return;
            }
            Payload::Move {
                node,
                new_parent,
                new_name: name,
            }
        }
        3 if !nodes.is_empty() => Payload::Delete {
            node: pick(&nodes, a),
        },
        4 | 5 if !files.is_empty() => {
            let blob = rep.put_blob(&[b % 4, a % 2]);
            Payload::SetContent {
                node: pick(&files, a),
                blob,
            }
        }
        6 => {
            // Buggy peer: §2.2-invalid op (unknown node).
            let op = rep.author(Payload::Delete {
                node: NodeId::new(10_000 + a as u64, 9),
            });
            invalid.insert(op.id);
            return;
        }
        7 => {
            // Own-chain violation (§2.6 #7, accepted by §4.5): deps = {}.
            rep.author_with_deps(
                Payload::Create {
                    kind: Kind::Directory,
                    parent: ROOT,
                    name,
                },
                BTreeSet::new(),
            );
            return;
        }
        _ => Payload::Create {
            kind: Kind::File,
            parent: pick(&dirs, b),
            name,
        },
    };
    rep.author(payload);
}

/// Partial / shuffled / duplicated pull `reps[ai] ← reps[bi]`, checking
/// §4.7 #2, #5, #6, #7, #8 on the way.
#[allow(clippy::too_many_arguments)]
fn partial_pull(
    reps: &mut [SyncReplica],
    ai: usize,
    bi: usize,
    hint: Hint,
    cut: u8,
    mode: u8,
    stale: &VersionVector,
    rng: &mut Lcg,
) -> Result<(), TestCaseError> {
    let all = union_store(reps);
    let have = make_have(&reps[ai], hint, stale, &all, rng);
    let b = reps[bi].clone();
    let a_before = ids(&reps[ai]);

    // #7: the delta does not depend on the hint.
    let d = delta(b.store(), &have);
    let d0 = delta(
        b.store(),
        &Have {
            frontier: have.frontier.clone(),
            vv_hint: None,
        },
    );
    prop_assert_eq!(&d, &d0, "delta depends on vv_hint");
    // §4.3: `<` order, disjoint from K, V_A ∪ Δ ⊇ V_B.
    prop_assert!(d.windows(2).all(|w| w[0].id < w[1].id));
    let mut cover: BTreeSet<OpId> = a_before.clone();
    cover.extend(d.iter().map(|o| o.id));
    prop_assert!(ids(&b).is_subset(&cover), "Δ incomplete");
    // #5: every <-prefix of Δ united with V_A is closed.
    let mut acc = reps[ai].store().clone();
    for o in &d {
        prop_assert!(
            o.deps.iter().all(|x| acc.contains_key(x)),
            "Δ prefix not closed at {:?}",
            o.id
        );
        acc.insert(o.id, o.clone());
    }

    let batches = b.answer(&have, 1 + (mode as usize % 3));
    let flat: Vec<Op> = batches.iter().flat_map(|m| m.ops.clone()).collect();
    prop_assert_eq!(&flat, &d);
    let blobs: Vec<(BlobId, Vec<u8>)> = batches.iter().flat_map(|m| m.blobs.clone()).collect();

    let a = &mut reps[ai];
    let k = cut as usize % (d.len() + 1);
    let shuffle = mode % 2 == 1;
    if shuffle {
        // #8: duplicated and shuffled delivery through the buffer.
        let mut seq: Vec<Op> = d[..k].to_vec();
        for _ in 0..k / 2 + 1 {
            if !d.is_empty() {
                seq.push(d[rng.below(d.len().min(k.max(1)))].clone());
            }
        }
        for i in (1..seq.len()).rev() {
            seq.swap(i, rng.below(i + 1));
        }
        for (id, bytes) in &blobs {
            a.receive_blob(ReplicaId(bi as u128 + 1), *id, bytes);
        }
        for o in seq {
            a.receive(ReplicaId(bi as u128 + 1), o);
            prop_assert!(closed(a.store()));
        }
    } else {
        for m in &batches {
            let mut m = m.clone();
            m.ops.retain(|o| d[..k].iter().any(|x| x.id == o.id));
            a.receive_ops(ReplicaId(bi as u128 + 1), &m);
            prop_assert!(closed(a.store()));
            prop_assert_eq!(a.pending_len(), 0, "single honest sender needs no buffer");
        }
    }
    // #2 monotone, #6 closed and ⊆ V_A ∪ V_B.
    let after = ids(a);
    prop_assert!(a_before.is_subset(&after));
    let mut ab = a_before.clone();
    ab.extend(ids(&b));
    prop_assert!(after.is_subset(&ab));
    prop_assert!(closed(a.store()));
    if k == d.len() {
        prop_assert_eq!(&after, &ab, "complete pull must reach V_A ∪ V_B");
        prop_assert_eq!(a.pending_len(), 0);
    }
    Ok(())
}

fn full_sync_checked(
    reps: &mut [SyncReplica],
    i: usize,
    j: usize,
    batch: usize,
) -> Result<(), TestCaseError> {
    let (fa, fb) = (reps[i].frontier(), reps[j].frontier());
    let mut join = ids(&reps[i]);
    join.extend(ids(&reps[j]));
    // #4 commutative: sync(B,A) on clones.
    let (mut a2, mut b2) = (reps[i].clone(), reps[j].clone());
    sync(&mut b2, &mut a2, batch);
    let (x, y) = two(reps, i, j);
    sync(x, y, batch);
    // #1 join and frontier max≺(F_A ∪ F_B).
    prop_assert_eq!(ids(x), join.clone());
    prop_assert_eq!(ids(y), join);
    let mf = merge_frontiers(x.store(), &fa, &fb);
    prop_assert_eq!(x.frontier(), mf.clone());
    prop_assert_eq!(y.frontier(), mf);
    prop_assert_eq!(ids(&a2), ids(x));
    prop_assert_eq!(ids(&b2), ids(y));
    prop_assert_eq!(x.pending_len() + y.pending_len(), 0);
    // #3 idempotent: second session applies nothing.
    let before = (ids(x), ids(y));
    sync(x, y, batch);
    prop_assert_eq!((ids(x), ids(y)), before);
    Ok(())
}

fn two(v: &mut [SyncReplica], i: usize, j: usize) -> (&mut SyncReplica, &mut SyncReplica) {
    assert_ne!(i, j);
    if i < j {
        let (l, r) = v.split_at_mut(j);
        (&mut l[i], &mut r[0])
    } else {
        let (l, r) = v.split_at_mut(i);
        (&mut r[0], &mut l[j])
    }
}

type Script = Vec<(u8, u8, u8, u8, u8)>;

fn schedule() -> impl Strategy<Value = (usize, Script, u64)> {
    (
        2usize..=4,
        prop::collection::vec(any::<(u8, u8, u8, u8, u8)>(), 0..64),
        any::<u64>(),
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn sync_properties_4_7((n, script, seed) in schedule()) {
        let mut reps: Vec<SyncReplica> =
            (1..=n as u128).map(|r| SyncReplica::with_id(ReplicaId(r))).collect();
        let mut stale: Vec<VersionVector> = vec![VersionVector::new(); n];
        let mut invalid = BTreeSet::new();
        let mut rng = Lcg(seed);
        for &(r, act, a, b, c) in &script {
            let ri = r as usize % n;
            let oi = (ri + 1 + a as usize % (n - 1)) % n;
            match act % 16 {
                0..=7 => author_random(&mut reps[ri], c, a, b, &mut invalid),
                8..=10 => {
                    partial_pull(&mut reps, ri, oi, hint_of(b), c, a ^ b, &stale[ri], &mut rng)?;
                }
                11 => full_sync_checked(&mut reps, ri, oi, 1 + b as usize % 4)?,
                12 => {
                    let rep = &mut reps[ri];
                    let cp = Checkpoint::new(rep.store(), &rep.version(), format!("c{b}"), rep.id(), None);
                    rep.publish(cp);
                }
                13 => {
                    // #10: checkpoint exchange changes no version.
                    let msg = reps[oi].checkpoints_msg();
                    let before = ids(&reps[ri]);
                    reps[ri].receive_checkpoints(&msg);
                    prop_assert_eq!(ids(&reps[ri]), before);
                    for cp in reps[ri].published() {
                        let pend = !cp.frontier().iter().all(|f| reps[ri].store().contains_key(f));
                        prop_assert_eq!(reps[ri].checkpoint_pending(cp), pend);
                    }
                }
                _ => stale[ri] = reps[ri].vv(),
            }
        }
        // Full schedule: two rounds of all pairs.
        for _ in 0..2 {
            for i in 0..n {
                for j in 0..n {
                    if i != j {
                        sync_pair(&mut reps, i, j);
                    }
                }
            }
        }
        // #4 convergence of everything.
        let all = union_store(&reps);
        let spec = evaluate(all.values());
        let r0 = &reps[0];
        let pub0: BTreeSet<String> = r0.published().iter().map(|c| format!("{c:?}")).collect();
        for r in &reps {
            prop_assert_eq!(ids(r), all.keys().copied().collect::<BTreeSet<_>>());
            prop_assert_eq!(r.pending_len(), 0);
            prop_assert!(r.warnings().is_empty(), "honest run warned: {:?}", r.warnings());
            prop_assert_eq!(r.frontier(), r0.frontier());
            prop_assert_eq!(r.version_id(), r0.version_id());
            prop_assert_eq!(r.version_id().short(), r0.version_id().short());
            prop_assert_eq!(&r.state(), &spec.state);
            prop_assert_eq!(&r.conflicts(), &spec.conflicts);
            prop_assert_eq!(r.projection(), r0.projection());
            // #9 invalid ops replicate with the same conflict.
            for id in &invalid {
                let inv = Conflict::InvalidOp { op: *id };
                prop_assert!(r.conflicts().contains(&inv), "missing invalid-op");
            }
            // #10 same published set; nothing pending once all ops are held.
            let p: BTreeSet<String> = r.published().iter().map(|c| format!("{c:?}")).collect();
            prop_assert_eq!(&p, &pub0);
            prop_assert!(r.published().iter().all(|c| !r.checkpoint_pending(c)));
            // Every blob referenced by a SetContent is present (blobs travel).
            for o in r.store().values() {
                if let Payload::SetContent { blob, .. } = &o.payload {
                    prop_assert!(r.blobs().contains(blob));
                }
            }
        }
    }
}

fn sync_pair(reps: &mut [SyncReplica], i: usize, j: usize) {
    let (x, y) = two(reps, i, j);
    sync(x, y, 2);
}

// ---------- explicit tests ----------

const A: u128 = 1;
const B: u128 = 2;
const C: u128 = 3;

fn id(c: u64, r: u128) -> OpId {
    NodeId::new(c, r)
}

/// §3.6 / §4.8 starting point.
fn worked() -> (SyncReplica, SyncReplica, SyncReplica) {
    let mut ra = SyncReplica::with_id(ReplicaId(A));
    let mut rb = SyncReplica::with_id(ReplicaId(B));
    let mut rc = SyncReplica::with_id(ReplicaId(C));
    let a1 = ra.author(Payload::Create {
        kind: Kind::Directory,
        parent: ROOT,
        name: "p".into(),
    });
    assert_eq!(a1.id, id(1, A));
    rb.receive(ReplicaId(A), a1.clone());
    rc.receive(ReplicaId(A), a1.clone());
    let a2 = ra.author(Payload::Move {
        node: a1.id,
        new_parent: ROOT,
        new_name: "q".into(),
    });
    assert_eq!(a2.id, id(2, A));
    let b1 = rb.author(Payload::Create {
        kind: Kind::File,
        parent: a1.id,
        name: "f".into(),
    });
    assert_eq!(b1.id, id(2, B));
    rc.receive(ReplicaId(A), a2);
    let c1 = rc.author(Payload::Create {
        kind: Kind::File,
        parent: a1.id,
        name: "g".into(),
    });
    assert_eq!(c1.id, id(3, C));
    (ra, rb, rc)
}

fn dids(d: &[Op]) -> Vec<OpId> {
    d.iter().map(|o| o.id).collect()
}

fn check_result(r: &SyncReplica) {
    assert_eq!(r.frontier(), vec![id(2, B), id(3, C)]);
    let vv: Vec<u64> = [A, B, C].iter().map(|x| r.vv()[&ReplicaId(*x)]).collect();
    assert_eq!(vv, vec![2, 2, 3]);
    assert_eq!(
        r.version_id().hex(),
        "ac652fa3bae659478269f983eb34f25b5ba1b4f3d6e169d773147e4c4fea5003"
    );
    assert_eq!(r.version_id().short(), "ac652fa3");
    assert!(r.conflicts().is_empty());
    let st = r.state();
    let live: BTreeSet<String> = st
        .values()
        .filter(|n| n.kind == Kind::File && is_live(&st, n.id))
        .map(|n| format!("{}/{}", st[&n.parent.unwrap()].name, n.name))
        .collect();
    assert_eq!(live, ["q/f".to_string(), "q/g".to_string()].into());
}

#[test]
fn worked_example_4_8_order_1() {
    let (mut ra, mut rb, mut rc) = worked();
    // A↔B concurrently: both Haves taken before either half applies.
    let ha = ra.have(true);
    assert_eq!(ha.vv_hint.as_ref().unwrap()[&ReplicaId(A)], 2);
    let hb = rb.have(true);
    assert_eq!(dids(&delta(rb.store(), &ha)), vec![id(1, A), id(2, B)]);
    assert_eq!(dids(&delta(ra.store(), &hb)), vec![id(1, A), id(2, A)]);
    for m in rb.answer(&ha, 10) {
        ra.receive_ops(ReplicaId(B), &m);
    }
    for m in ra.clone().answer(&hb, 10) {
        rb.receive_ops(ReplicaId(A), &m);
    }
    assert_eq!(ra.frontier(), vec![id(2, A), id(2, B)]);
    assert_eq!(ra.version_id().short(), "aadfb58d");
    assert_eq!(rb.frontier(), ra.frontier());
    // B↔C: pull C←B then B←C sequentially.
    assert_eq!(
        dids(&delta(rb.store(), &rc.have(false))),
        vec![id(1, A), id(2, A), id(2, B)]
    );
    pull(&mut rc, &rb, 10, true);
    assert_eq!(dids(&delta(rc.store(), &rb.have(false))), vec![id(3, C)]);
    pull(&mut rb, &rc, 10, true);
    // C↔A.
    assert_eq!(dids(&delta(rc.store(), &ra.have(false))), vec![id(3, C)]);
    pull(&mut ra, &rc, 10, true);
    assert!(delta(ra.store(), &rc.have(false)).is_empty());
    for r in [&ra, &rb, &rc] {
        check_result(r);
    }
}

#[test]
fn worked_example_4_8_order_2_stale_hint_and_interruption() {
    let (ra0, mut rb, mut rc) = worked();
    assert_eq!(
        dids(&delta(rb.store(), &rc.have(false))),
        vec![id(1, A), id(2, B)]
    );
    pull(&mut rc, &rb, 10, true);
    assert_eq!(
        dids(&delta(rc.store(), &rb.have(false))),
        vec![id(2, A), id(3, C)]
    );
    pull(&mut rb, &rc, 10, true);

    // A↔C with the true hint, a stale hint (1,0,0) and a too-high hint (2,2,3).
    let hints: Vec<Option<VersionVector>> = vec![
        Some([(ReplicaId(A), 2)].into()),
        Some([(ReplicaId(A), 1)].into()),
        Some([(ReplicaId(A), 2), (ReplicaId(B), 2), (ReplicaId(C), 3)].into()),
        None,
    ];
    for h in hints {
        let have = Have {
            frontier: ra0.frontier(),
            vv_hint: h,
        };
        assert_eq!(dids(&delta(rc.store(), &have)), vec![id(2, B), id(3, C)]);
    }
    let mut ra = ra0.clone();
    pull(&mut ra, &rc, 10, true);
    assert!(delta(ra.store(), &rc.have(false)).is_empty());
    for r in [&ra, &rb, &rc] {
        check_result(r);
    }

    // Interruption: pull A←C cut after b1, then resume.
    let mut ra = ra0.clone();
    let batches = rc.answer(&ra.have(true), 1);
    ra.receive_ops(ReplicaId(C), &batches[0]);
    assert_eq!(ra.frontier(), vec![id(2, A), id(2, B)]);
    let resume = ra.have(true);
    assert_eq!(dids(&delta(rc.store(), &resume)), vec![id(3, C)]);
    pull(&mut ra, &rc, 1, true);
    check_result(&ra);
}

fn mkdir_op(c: u64, r: u128, deps: &[OpId], name: &str) -> Op {
    Op {
        id: id(c, r),
        deps: deps.iter().copied().collect(),
        payload: Payload::Create {
            kind: Kind::Directory,
            parent: ROOT,
            name: name.into(),
        },
    }
}

#[test]
fn equivocating_peer_is_rejected_with_warning() {
    let mut r = SyncReplica::with_id(ReplicaId(5));
    let good = mkdir_op(1, 7, &[], "x");
    let evil = mkdir_op(1, 7, &[], "y");
    r.receive(ReplicaId(7), good.clone());
    r.receive(ReplicaId(8), good.clone()); // same payload: silent no-op
    assert!(r.warnings().is_empty());
    r.receive(ReplicaId(8), evil);
    assert_eq!(r.store()[&good.id], good, "keep-first");
    let w = r.warnings();
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].peer, ReplicaId(8));
    assert_eq!(w[0].rejected, Some(good.id));
    assert_eq!(w[0].author, Some(ReplicaId(7)));
    assert_eq!(w[0].reason, RejectReason::Equivocation);
    // Also while the first copy is still pending.
    let mut r = SyncReplica::with_id(ReplicaId(5));
    let dep = mkdir_op(1, 7, &[], "d");
    r.receive(ReplicaId(7), mkdir_op(2, 7, &[dep.id], "x"));
    r.receive(ReplicaId(8), mkdir_op(2, 7, &[dep.id], "y"));
    assert_eq!(r.warnings()[0].reason, RejectReason::Equivocation);
    r.receive(ReplicaId(7), dep);
    assert_eq!(r.state().values().filter(|n| n.name == "x").count(), 1);
}

#[test]
fn lamport_violation_is_rejected_with_stuck_descendants() {
    let mut r = SyncReplica::with_id(ReplicaId(5));
    let d = mkdir_op(5, 7, &[], "d");
    let bad = mkdir_op(3, 8, &[d.id], "bad"); // dep (5,7) ≮ (3,8)
    let child = mkdir_op(9, 8, &[bad.id], "child");
    r.receive(ReplicaId(8), child.clone());
    r.receive(ReplicaId(8), bad.clone());
    r.receive(ReplicaId(7), d.clone());
    assert!(r.store().contains_key(&d.id));
    assert!(!r.store().contains_key(&bad.id));
    assert!(!r.store().contains_key(&child.id));
    let w = r.warnings();
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].reason, RejectReason::LamportViolation);
    assert_eq!(w[0].peer, ReplicaId(8));
    assert_eq!(w[0].rejected, Some(bad.id));
    assert_eq!(w[0].stuck, vec![child.id]);
    // Never advertised; a re-send is a no-op.
    assert_eq!(r.frontier(), vec![d.id]);
    r.receive(ReplicaId(8), bad);
    assert_eq!(r.warnings().len(), 1);
}

#[test]
fn pending_ops_apply_once_deps_arrive() {
    let mut src = SyncReplica::with_id(ReplicaId(1));
    let a = src.author(Payload::Create {
        kind: Kind::Directory,
        parent: ROOT,
        name: "a".into(),
    });
    let b = src.author(Payload::Create {
        kind: Kind::File,
        parent: a.id,
        name: "b".into(),
    });
    let blob = src.put_blob(b"hello");
    let c = src.author(Payload::SetContent { node: b.id, blob });
    let mut r = SyncReplica::with_id(ReplicaId(2));
    r.receive(ReplicaId(1), c.clone());
    r.receive(ReplicaId(1), b.clone());
    assert_eq!(r.pending_len(), 2);
    assert!(r.store().is_empty());
    let need = r.need(&src.frontier());
    assert_eq!(need.ids, vec![a.id]);
    r.receive_ops(ReplicaId(1), &src.answer_need(&need));
    // c still waits for its blob.
    assert_eq!(r.pending_len(), 1);
    assert!(r.warnings().is_empty());
    assert!(r.receive_blob(ReplicaId(1), blob, b"hello"));
    assert_eq!(r.pending_len(), 0);
    assert_eq!(r.store(), src.store());
    assert_eq!(r.state(), src.state());
}

#[test]
fn corrupt_blob_is_rejected() {
    let mut src = SyncReplica::with_id(ReplicaId(1));
    let f = src.author(Payload::Create {
        kind: Kind::File,
        parent: ROOT,
        name: "f".into(),
    });
    let blob = src.put_blob(b"real");
    let e = src.author(Payload::SetContent { node: f.id, blob });
    let mut r = SyncReplica::with_id(ReplicaId(2));
    let mut msgs = src.answer(&r.have(false), 10);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].blobs, vec![(blob, b"real".to_vec())]);
    msgs[0].blobs[0].1 = b"fake".to_vec();
    r.receive_ops(ReplicaId(9), &msgs[0]);
    assert!(!r.blobs().contains(&blob));
    assert!(
        !r.store().contains_key(&e.id),
        "SetContent waits for a valid blob"
    );
    assert!(r.store().contains_key(&f.id));
    let w = r.warnings();
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].reason, RejectReason::CorruptBlob);
    assert_eq!(w[0].peer, ReplicaId(9));
    assert_eq!(w[0].stuck, vec![e.id]);
    // A correct copy later unblocks it.
    pull(&mut r, &src, 10, false);
    assert_eq!(r.store(), src.store());
    assert_eq!(r.state()[&f.id].content, Some(blob));
}

#[test]
fn invalid_op_is_accepted_and_recorded() {
    let mut a = SyncReplica::with_id(ReplicaId(1));
    let mut b = SyncReplica::with_id(ReplicaId(2));
    let bad = a.author(Payload::Delete { node: id(99, 42) });
    sync(&mut a, &mut b, 1);
    for r in [&a, &b] {
        assert!(r.store().contains_key(&bad.id));
        assert!(r.conflicts().contains(&Conflict::InvalidOp { op: bad.id }));
        assert!(r.warnings().is_empty());
    }
}

#[test]
fn replica_ids_are_random_nonzero_and_duplicates_are_detected() {
    let ids: BTreeSet<ReplicaId> = (0..64).map(|_| new_replica_id()).collect();
    assert_eq!(ids.len(), 64);
    assert!(!ids.contains(&ReplicaId::ZERO));
    assert_ne!(SyncReplica::new().id(), SyncReplica::new().id());

    // A copied `.hub/`: two replicas share id 7 and both author (1,7).
    let mut orig = SyncReplica::with_id(ReplicaId(7));
    let mut copy = orig.clone();
    orig.author(Payload::Create {
        kind: Kind::Directory,
        parent: ROOT,
        name: "o".into(),
    });
    copy.author(Payload::Create {
        kind: Kind::Directory,
        parent: ROOT,
        name: "c".into(),
    });
    let mut third = SyncReplica::with_id(ReplicaId(3));
    // Detected only when both copies of (1,7) reach one replica: here two
    // concurrent pulls answered to the same (genesis) Have. Sequential pulls
    // would not see it: after the first, K = downclose({(1,7)}) hides it.
    let have = third.have(false);
    for m in orig.answer(&have, 10) {
        third.receive_ops(orig.id(), &m);
    }
    for m in copy.answer(&have, 10) {
        third.receive_ops(ReplicaId(8), &m);
    }
    assert_eq!(third.warnings()[0].peer, ReplicaId(8));
    assert_eq!(third.warnings()[0].reason, RejectReason::Equivocation);
    // Directly, equal frontier ids hide the fork (§3.1: a frontier does not
    // detect OpId reuse): Δ = ∅ and no warning.
    pull(&mut orig, &copy, 10, false);
    assert!(orig.warnings().is_empty());
    // The copy's next op arrives under the original's own id, which it never
    // authored: DuplicateReplica (accepted, only flagged).
    let op2 = copy.author(Payload::Create {
        kind: Kind::Directory,
        parent: ROOT,
        name: "c2".into(),
    });
    pull(&mut orig, &copy, 10, false);
    let w = orig.warnings();
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].reason, RejectReason::DuplicateReplica);
    assert_eq!(w[0].rejected, Some(op2.id));
    assert!(orig.store().contains_key(&op2.id));
}

#[test]
fn warnings_are_local() {
    let mut a = SyncReplica::with_id(ReplicaId(1));
    let mut b = SyncReplica::with_id(ReplicaId(2));
    a.receive(ReplicaId(9), mkdir_op(1, 7, &[], "x"));
    a.receive(ReplicaId(9), mkdir_op(1, 7, &[], "y"));
    assert_eq!(a.warnings().len(), 1);
    sync(&mut a, &mut b, 3);
    assert_eq!(a.warnings().len(), 1);
    assert!(b.warnings().is_empty());
}
