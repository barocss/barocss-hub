//! Issue #28: the scalable `evaluate` / `Causality` / batched `Replica`
//! equal the pre-#28 oracle (`tree::oracle`, materialized `causal_past`) on
//! State, Conflicts, outcomes and Projection, for the shared #5 generator's
//! histories plus dep mutations that violate §2.6 #7 (own-chain), add
//! extra/missing/non-Lamport deps, and drop ops.

use std::collections::{BTreeMap, BTreeSet};

use hub_core::projection::project;
use hub_core::tree::*;
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

/// Causally closed, Lamport-consistent mutation: each op may lose some deps
/// (own-chain violations) and gain deps on random smaller ops.
fn mutate_closed(ops: &[Op], seed: u64) -> Vec<Op> {
    let mut rng = Xs(seed | 1);
    let ids: Vec<OpId> = {
        let mut v: Vec<OpId> = ops.iter().map(|o| o.id).collect();
        v.sort();
        v
    };
    ops.iter()
        .map(|o| {
            let mut o = o.clone();
            match rng.below(4) {
                0 => o.deps.retain(|_| rng.below(2) == 0),
                1 => o.deps.clear(),
                2 => {
                    let smaller = ids.partition_point(|i| *i < o.id);
                    if smaller > 0 {
                        o.deps.insert(ids[rng.below(smaller)]);
                    }
                }
                _ => {}
            }
            o
        })
        .collect()
}

/// Arbitrary op set (evaluate is a pure function of any set): additionally
/// drop ops, add deps on missing ids and on ids `>=` the op (both ignored).
fn mutate_open(ops: &[Op], seed: u64) -> Vec<Op> {
    let mut rng = Xs(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let ids: Vec<OpId> = ops.iter().map(|o| o.id).collect();
    let mut out = Vec::new();
    for mut o in mutate_closed(ops, seed ^ 0x55) {
        if rng.below(8) == 0 {
            continue;
        }
        {
            match rng.below(4) {
                0 => {
                    o.deps
                        .insert(NodeId::new(o.id.counter + 1 + rng.below(3) as u64, 1));
                }
                1 if !ids.is_empty() => {
                    o.deps.insert(ids[rng.below(ids.len())]);
                }
                2 => {
                    o.deps.insert(o.id);
                }
                _ => {}
            }
            out.push(o);
        }
    }
    out
}

fn same(ops: &[Op]) -> Result<(), TestCaseError> {
    let new = evaluate(ops);
    let old = oracle::evaluate(ops);
    prop_assert_eq!(&new.state, &old.state);
    prop_assert_eq!(&new.conflicts, &old.conflicts);
    prop_assert_eq!(&new.outcomes, &old.outcomes);
    prop_assert_eq!(project(&new.state), project(&old.state));
    Ok(())
}

fn same_causality(ops: &[Op]) -> Result<(), TestCaseError> {
    let map: BTreeMap<OpId, Op> = ops.iter().map(|o| (o.id, o.clone())).collect();
    let past = causal_past(&map);
    let c = Causality::new(&map);
    for &a in map.keys() {
        for &b in map.keys() {
            prop_assert_eq!(c.precedes(a, b), past[&b].contains(&a), "{:?} ≺ {:?}", a, b);
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    #[test]
    fn evaluate_equals_oracle((nrep, script, seeds) in script_strategy()) {
        let ops = simulate(nrep, &script);
        same(&ops)?;
        same_causality(&ops)?;
        let closed = mutate_closed(&ops, seeds[0]);
        same(&closed)?;
        same_causality(&closed)?;
        let open = mutate_open(&ops, seeds[1]);
        same(&open)?;
        same_causality(&open)?;
        // Incremental replica (batched view refresh) under shuffled,
        // duplicated delivery of the closed mutation, one batch and one by
        // one, equals the oracle state.
        let spec = oracle::evaluate(&closed);
        let mut batch = Replica::new();
        batch.receive_all(delivery(&closed, seeds[2]));
        prop_assert_eq!(batch.pending_len(), 0);
        prop_assert_eq!(batch.state(), &spec.state);
        let mut single = Replica::new();
        for op in delivery(&closed, seeds[2] ^ 0xabcd) {
            single.receive(op);
        }
        prop_assert_eq!(single.state(), &spec.state);
    }
}

/// Deterministic sample: the equivalence inputs really contain own-chain
/// violations, cycles, revives (D-001 and D-014), content, same-node-move
/// and invalid-op conflicts.
#[test]
fn equivalence_inputs_cover_rules() {
    use hub_core::version::own_chain_ok;
    use proptest::strategy::ValueTree;
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
    let mut runner = TestRunner::new_with_rng(
        Config::default(),
        TestRng::from_seed(RngAlgorithm::ChaCha, &[28; 32]),
    );
    let strat = script_strategy();
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    let n = 1024;
    for _ in 0..n {
        let (nrep, script, seeds) = strat.new_tree(&mut runner).unwrap().current();
        let ops = simulate(nrep, &script);
        let closed = mutate_closed(&ops, seeds[0]);
        let store: BTreeMap<OpId, Op> = closed.iter().map(|o| (o.id, o.clone())).collect();
        *seen.entry("own-chain violation").or_default() += !own_chain_ok(&store) as usize;
        let mut kinds = BTreeSet::new();
        for v in [&ops, &closed, &mutate_open(&ops, seeds[1])] {
            for c in &evaluate(v).conflicts {
                kinds.insert(match c {
                    Conflict::InvalidOp { .. } => "invalid-op",
                    Conflict::Cycle { .. } => "cycle",
                    Conflict::SameNodeMove { .. } => "same-node-move",
                    Conflict::Revived { .. } => "revived",
                    Conflict::Content { .. } => "content",
                    Conflict::EditRevived { .. } => "edit-revived",
                });
            }
        }
        for k in kinds {
            *seen.entry(k).or_default() += 1;
        }
    }
    eprintln!("equivalence coverage over {n} cases: {seen:?}");
    for k in [
        "own-chain violation",
        "invalid-op",
        "cycle",
        "same-node-move",
        "revived",
        "content",
        "edit-revived",
    ] {
        assert!(
            seen.get(k).copied().unwrap_or(0) * 100 >= n,
            "{k} below 1%: {seen:?}"
        );
    }
}

/// §2.6 #7 fallback: VV-≤ says "after" but the ops are concurrent.
#[test]
fn own_chain_violation_is_not_treated_as_ordered() {
    let a1 = Op {
        id: NodeId::new(2, 1),
        deps: BTreeSet::new(),
        payload: Payload::Create {
            kind: Kind::File,
            parent: ROOT,
            name: "f".into(),
        },
    };
    // a3 by replica 1 skips a2 (own-chain violation); b depends on a3 only.
    let a2 = Op {
        id: NodeId::new(3, 1),
        deps: BTreeSet::from([a1.id]),
        payload: Payload::SetContent {
            node: a1.id,
            blob: BlobId::of(b"x"),
        },
    };
    let a3 = Op {
        id: NodeId::new(4, 1),
        deps: BTreeSet::from([a1.id]),
        payload: Payload::SetContent {
            node: a1.id,
            blob: BlobId::of(b"y"),
        },
    };
    let ops = vec![a1.clone(), a2.clone(), a3.clone()];
    let map: BTreeMap<OpId, Op> = ops.iter().map(|o| (o.id, o.clone())).collect();
    let c = Causality::new(&map);
    assert!(c.precedes(a1.id, a3.id));
    assert!(
        !c.precedes(a2.id, a3.id),
        "VV says a2 ≤ a3, but it is not in deps*"
    );
    assert!(c.concurrent(a2.id, a3.id));
    let ev = evaluate(&ops);
    assert!(ev
        .conflicts
        .iter()
        .any(|c| matches!(c, Conflict::Content { loser, .. } if *loser == a2.id)));
    assert_eq!(ev.conflicts, oracle::evaluate(&ops).conflicts);
}
