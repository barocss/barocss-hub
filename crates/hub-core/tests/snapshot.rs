//! Issue #33: `Replica::encode_snapshot` / `decode_snapshot` round-trip.
//! A decoded replica equals the replayed one on state, conflicts, op log
//! and further behaviour (receiving the rest of the history, including ops
//! smaller than the snapshot's last op, and `state_with_tail`), for the
//! shared histories and the closed mutation. Plus the #32 addendum: the
//! `state_with_tail` guard against a tail op not greater than every
//! applied op.

use std::collections::{BTreeMap, BTreeSet};

use hub_core::tree::*;
use proptest::prelude::*;

mod common;
use common::*;

fn store_of(r: &Replica) -> BTreeMap<OpId, Op> {
    r.ops().map(|o| (o.id, o.clone())).collect()
}

fn check(ops: &[Op], seed: u64) -> Result<(), TestCaseError> {
    let mut sorted: Vec<Op> = ops.to_vec();
    sorted.sort_by_key(|o| o.id);
    sorted.dedup_by_key(|o| o.id);
    // Only closed prefixes can be snapshotted.
    let k = (seed as usize) % (sorted.len() + 1);
    let mut r = Replica::new();
    r.receive_all(sorted[..k].to_vec());
    let Some(img) = r.encode_snapshot() else {
        prop_assert!(r.pending_len() > 0);
        return Ok(());
    };
    let mut d = Replica::decode_snapshot(&img).map_err(TestCaseError::fail)?;
    prop_assert_eq!(
        d.encode_snapshot().unwrap(),
        img.clone(),
        "re-encode differs"
    );
    prop_assert_eq!(r.state(), d.state());
    prop_assert_eq!(r.len(), d.len());
    prop_assert!(r.ops().eq(d.ops()));
    let s = store_of(&r);
    prop_assert_eq!(r.conflicts(&s), d.conflicts(&s));
    // Truncated images are refused.
    if img.len() > 8 {
        prop_assert!(Replica::decode_snapshot(&img[..img.len() - 1]).is_err());
    }
    // Tail preview on both.
    prop_assert_eq!(
        r.state_with_tail(&sorted[k..]),
        d.state_with_tail(&sorted[k..])
    );
    // The rest of the history, shuffled and duplicated (smaller ids too).
    let rest = delivery(ops, seed ^ 0xabc);
    r.receive_all(rest.clone());
    d.receive_all(rest);
    prop_assert_eq!(r.state(), d.state());
    prop_assert_eq!(r.pending_len(), d.pending_len());
    let s = store_of(&r);
    prop_assert_eq!(r.conflicts(&s), d.conflicts(&s));
    if r.pending_len() == 0 {
        let e = evaluate(s.values());
        prop_assert_eq!(d.state(), &e.state);
        prop_assert_eq!(d.conflicts(&s), Some(e.conflicts));
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn snapshot_round_trip_equals_replay((nrep, script, seeds) in script_strategy()) {
        let ops = simulate(nrep, &script);
        check(&ops, seeds[0])?;
        check(&ops, seeds[1])?;
    }
}

fn op(c: u64, r: u128, deps: &[OpId], payload: Payload) -> Op {
    Op {
        id: NodeId::new(c, r),
        deps: deps.iter().copied().collect::<BTreeSet<_>>(),
        payload,
    }
}

/// #32 addendum: a tail op not greater than the last applied op makes
/// `state_with_tail` return `None` (a concurrent lower-id tail would
/// otherwise be applied after a greater op and diverge from the fold).
#[test]
fn state_with_tail_refuses_a_lower_tail_op() {
    let x = NodeId::new(2, 1);
    let a = NodeId::new(3, 1);
    let b = NodeId::new(4, 1);
    let base = vec![
        op(
            2,
            1,
            &[],
            Payload::Create {
                kind: Kind::File,
                parent: ROOT,
                name: "x".into(),
            },
        ),
        op(
            3,
            1,
            &[x],
            Payload::Create {
                kind: Kind::Directory,
                parent: ROOT,
                name: "a".into(),
            },
        ),
        op(
            4,
            1,
            &[a],
            Payload::Create {
                kind: Kind::Directory,
                parent: ROOT,
                name: "b".into(),
            },
        ),
        // Greatest applied op: move x under /a.
        op(
            9,
            1,
            &[b],
            Payload::Move {
                node: x,
                new_parent: a,
                new_name: "x".into(),
            },
        ),
    ];
    let mut r = Replica::new();
    r.receive_all(base.clone());
    // Concurrent move of x under /b with a smaller id (replica 2).
    let low = op(
        5,
        2,
        &[b],
        Payload::Move {
            node: x,
            new_parent: b,
            new_name: "x".into(),
        },
    );
    assert_eq!(r.state_with_tail(std::slice::from_ref(&low)), None);
    // Without the guard the preview would put x under /b; the fold puts it
    // under /a (the greater move wins).
    let all: Vec<Op> = base.iter().cloned().chain([low.clone()]).collect();
    let e = evaluate(&all);
    assert_eq!(e.state[&x].parent, Some(a));
    // Equal id (already applied) is refused too; a greater tail is previewed.
    assert_eq!(r.state_with_tail(std::slice::from_ref(&base[3])), None);
    let hi = op(
        10,
        2,
        &[NodeId::new(9, 1)],
        Payload::Move {
            node: x,
            new_parent: b,
            new_name: "x".into(),
        },
    );
    let st = r
        .state_with_tail(std::slice::from_ref(&hi))
        .expect("greater tail previews");
    assert_eq!(st[&x].parent, Some(b));
}
