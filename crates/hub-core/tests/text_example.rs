//! Issue #53: the Text-CRDT 7.13 worked example (2 agents + 1 human, one
//! file) and its variants, as deterministic unit tests with the exact
//! texts and op ids; plus `hub-diff-line/1` golden vectors (7.12 #8).

use std::collections::{BTreeMap, BTreeSet};

use hub_core::text::*;
use hub_core::tree::*;

mod common;

const SEED: &[u8] = b"# Plan\n- a\n- b\n";

fn id(c: u64, r: u128) -> OpId {
    NodeId::new(c, r)
}

fn e(op: OpId, off: u32) -> ElemId {
    ElemId { op, off }
}

fn deps(v: &[OpId]) -> BTreeSet<OpId> {
    v.iter().copied().collect()
}

/// Node `n` = (5,3) created by H; `S` = (10,3) its SetContent.
fn base() -> (Vec<Op>, NodeId, OpId, BTreeMap<BlobId, Vec<u8>>) {
    let n = id(5, 3);
    let s = id(10, 3);
    let blob = BlobId::of(SEED);
    let ops = vec![
        Op {
            id: n,
            deps: deps(&[]),
            payload: Payload::Create {
                kind: Kind::File,
                parent: ROOT,
                name: "notes.md".into(),
            },
        },
        Op {
            id: s,
            deps: deps(&[n]),
            payload: Payload::SetContent { node: n, blob },
        },
    ];
    let mut blobs = BTreeMap::new();
    blobs.insert(blob, SEED.to_vec());
    (ops, n, s, blobs)
}

fn te(oid: OpId, n: NodeId, s: OpId, del: Vec<(ElemId, u32)>, ins: Vec<Ins>) -> Op {
    Op {
        id: oid,
        deps: deps(&[s]),
        payload: Payload::TextEdit(TextEdit {
            node: n,
            epoch: s,
            seen: deps(&[s]),
            del,
            ins,
        }),
    }
}

fn ins(left: Option<ElemId>, right: Option<ElemId>, b: &str) -> Ins {
    Ins {
        left,
        right,
        bytes: b.as_bytes().to_vec(),
    }
}

/// The three concurrent ops of 7.13.
fn example_ops(n: NodeId, s: OpId) -> Vec<Op> {
    vec![
        te(
            id(11, 1),
            n,
            s,
            vec![],
            vec![ins(Some(e(s, 10)), Some(e(s, 11)), "- a1\n")],
        ),
        te(
            id(11, 2),
            n,
            s,
            vec![],
            vec![ins(Some(e(s, 10)), Some(e(s, 11)), "- a2\n")],
        ),
        te(
            id(11, 3),
            n,
            s,
            vec![(e(s, 11), 4)],
            vec![ins(Some(e(s, 14)), None, "- B\n")],
        ),
    ]
}

fn permutations(v: &[Op]) -> Vec<Vec<Op>> {
    if v.len() <= 1 {
        return vec![v.to_vec()];
    }
    let mut out = Vec::new();
    for i in 0..v.len() {
        let mut rest = v.to_vec();
        let x = rest.remove(i);
        for mut p in permutations(&rest) {
            p.insert(0, x.clone());
            out.push(p);
        }
    }
    out
}

fn head(ev: &Evaluation, n: NodeId) -> Vec<u8> {
    ev.text.content[&n].bytes.clone().unwrap()
}

#[test]
fn worked_example_all_six_orders() {
    let (mut ops, n, s, blobs) = base();
    let ex = example_ops(n, s);
    ops.extend(ex.clone());
    let want = b"# Plan\n- a\n- a1\n- a2\n- B\n";
    for p in permutations(&ex) {
        let mut order: Vec<OpId> = vec![n, s];
        order.extend(p.iter().map(|o| o.id));
        let ev = evaluate_in_order(&ops, &blobs, &order);
        assert_eq!(head(&ev, n), want, "order {order:?}");
        assert!(ev.conflicts.is_empty(), "{:?}", ev.conflicts);
        assert_eq!(
            ev.text.notices,
            [Notice::ConcurrentEdit {
                node: n,
                ops: vec![id(11, 1), id(11, 2), id(11, 3)],
                lines: (3, 5),
            }]
            .into_iter()
            .collect()
        );
        assert_eq!(ev.state[&n].content, Some(BlobId::of(want)));
    }
    // Tree shape (7.13 steps 1-3): full list with tombstones.
    let ev = evaluate_with(ops.iter(), &blobs);
    let doc = ev.text.doc(n, s).unwrap();
    let els: Vec<ElemId> = doc.elements().iter().map(|x| x.0).collect();
    let pos = |x: ElemId| els.iter().position(|y| *y == x).unwrap();
    assert!(pos(e(s, 10)) < pos(e(id(11, 1), 0)));
    assert!(pos(e(id(11, 1), 4)) < pos(e(id(11, 2), 0)));
    assert!(pos(e(id(11, 2), 4)) < pos(e(s, 11)));
    assert!(pos(e(s, 14)) < pos(e(id(11, 3), 0)));
    // Oracle agrees.
    let or = hub_core::text::oracle::evaluate(&ops, &blobs);
    assert_eq!(or.content[&n].1, want);
    assert_eq!(or.notices, ev.text.notices);
    assert_eq!(or.conflicts, ev.conflicts);
    // A later save by anyone with deps ⊇ all three clears the notice.
    let mut ops2 = ops.clone();
    ops2.push(Op {
        id: id(12, 3),
        deps: deps(&[id(11, 1), id(11, 2), id(11, 3)]),
        payload: Payload::TextEdit(TextEdit {
            node: n,
            epoch: s,
            seen: deps(&[id(11, 1), id(11, 2), id(11, 3)]),
            del: vec![],
            ins: vec![ins(None, Some(e(s, 0)), "top\n")],
        }),
    });
    let ev2 = evaluate_with(ops2.iter(), &blobs);
    assert!(ev2.text.notices.is_empty());
    assert_eq!(head(&ev2, n), b"top\n# Plan\n- a\n- a1\n- a2\n- B\n");
}

/// The three ops are exactly what `hub-diff-line/1` authors from V0.
#[test]
fn worked_example_ops_are_authored_by_the_diff() {
    let (ops, n, s, blobs) = base();
    let v0: BTreeMap<OpId, Op> = ops.iter().map(|o| (o.id, o.clone())).collect();
    let ev = evaluate_with(v0.values(), &blobs);
    let want = example_ops(n, s);
    for (saved, w) in [
        (&b"# Plan\n- a\n- a1\n- b\n"[..], &want[0]),
        (b"# Plan\n- a\n- a2\n- b\n", &want[1]),
        (b"# Plan\n- a\n- B\n", &want[2]),
    ] {
        let t = author_text_edit(&v0, &ev, &blobs, n, saved).unwrap();
        assert_eq!(Payload::TextEdit(t), w.payload);
    }
}

/// Variant (a): H deletes /notes.md concurrently; the agents' edits revive
/// it with one edit-revived conflict.
#[test]
fn worked_example_variant_a_delete_revives() {
    let (mut ops, n, s, blobs) = base();
    let ex = example_ops(n, s);
    ops.extend(ex[..2].iter().cloned());
    let del = Op {
        id: id(11, 3),
        deps: deps(&[s]),
        payload: Payload::Delete { node: n },
    };
    ops.push(del);
    for p in permutations(&ops[2..]) {
        let mut order = vec![n, s];
        order.extend(p.iter().map(|o| o.id));
        let ev = evaluate_in_order(&ops, &blobs, &order);
        assert!(is_live(&ev.state, n));
        assert_eq!(ev.state[&n].parent, Some(ROOT));
        assert_eq!(ev.state[&n].name, "notes.md");
        assert_eq!(head(&ev, n), b"# Plan\n- a\n- a1\n- a2\n- b\n");
        let revived: Vec<&Conflict> = ev
            .conflicts
            .iter()
            .filter(|c| matches!(c, Conflict::EditRevived { .. }))
            .collect();
        assert_eq!(
            revived,
            vec![&Conflict::EditRevived {
                node: n,
                edit: id(11, 1),
                delete: id(11, 3),
            }]
        );
        assert_eq!(ev.conflicts.len(), 1);
        // The two inserts at the same position still overlap (7.8).
        assert_eq!(
            ev.text.notices,
            [Notice::ConcurrentEdit {
                node: n,
                ops: vec![id(11, 1), id(11, 2)],
                lines: (3, 4),
            }]
            .into_iter()
            .collect()
        );
    }
}

/// Variant (b): H saves while still showing V0 after (11,1) arrived; and
/// A2 saves from V0 after (11,1) arrived but before checking it out (id
/// (12,2)). Same ops (modulo A2's id), same result.
#[test]
fn worked_example_variant_b_stale_view() {
    let (ops, n, s, blobs) = base();
    let ex = example_ops(n, s);
    let v0: BTreeMap<OpId, Op> = ops.iter().map(|o| (o.id, o.clone())).collect();
    let ev0 = evaluate_with(v0.values(), &blobs);
    // H's store holds (11,1); its index base is V0, so deps = V0.
    let h = author_text_edit(&v0, &ev0, &blobs, n, b"# Plan\n- a\n- B\n").unwrap();
    assert_eq!(h.seen, deps(&[s]));
    assert_eq!(Payload::TextEdit(h.clone()), ex[2].payload);
    // A2: counter = max_counter(store) + 1 = 12 since (11,1) is stored.
    let store_max = ops
        .iter()
        .chain([&ex[0]])
        .map(|o| o.id.counter)
        .max()
        .unwrap();
    let a2_id = id(store_max + 1, 2);
    assert_eq!(a2_id, id(12, 2));
    let a2 = author_text_edit(&v0, &ev0, &blobs, n, b"# Plan\n- a\n- a2\n- b\n").unwrap();
    assert_eq!(a2.ins, vec![ins(Some(e(s, 10)), Some(e(s, 11)), "- a2\n")]);
    let mut all = ops.clone();
    all.push(ex[0].clone());
    all.push(Op {
        id: a2_id,
        deps: deps(&[s]),
        payload: Payload::TextEdit(a2),
    });
    all.push(Op {
        id: id(11, 3),
        deps: deps(&[s]),
        payload: Payload::TextEdit(h),
    });
    for p in permutations(&all[2..]) {
        let mut order = vec![n, s];
        order.extend(p.iter().map(|o| o.id));
        let ev = evaluate_in_order(&all, &blobs, &order);
        assert_eq!(head(&ev, n), b"# Plan\n- a\n- a1\n- a2\n- B\n");
        assert!(ev.conflicts.is_empty());
    }
}

/// Variant (c): A2 is a pre-#48 peer; its whole-file save is a new epoch
/// and a `content` loser with its blob.
#[test]
fn worked_example_variant_c_old_peer() {
    let (mut ops, n, s, mut blobs) = base();
    let ex = example_ops(n, s);
    let a2_text = b"# Plan\n- a\n- a2\n- b\n";
    let a2_blob = BlobId::of(a2_text);
    blobs.insert(a2_blob, a2_text.to_vec());
    ops.push(ex[0].clone());
    ops.push(Op {
        id: id(11, 2),
        deps: deps(&[s]),
        payload: Payload::SetContent {
            node: n,
            blob: a2_blob,
        },
    });
    ops.push(ex[2].clone());
    for p in permutations(&ops[2..]) {
        let mut order = vec![n, s];
        order.extend(p.iter().map(|o| o.id));
        let ev = evaluate_in_order(&ops, &blobs, &order);
        assert_eq!(head(&ev, n), b"# Plan\n- a\n- a1\n- B\n");
        // winner_blob: the winner's epoch text at deps*(winner) ∪ {winner}.
        let wb = BlobId::of(b"# Plan\n- a\n- B\n");
        assert_eq!(
            ev.conflicts,
            [Conflict::Content {
                node: n,
                winner: id(11, 3),
                loser: id(11, 2),
                winner_blob: wb,
                loser_blob: a2_blob,
            }]
            .into_iter()
            .collect()
        );
        assert_eq!(ev.text.blobs[&wb], b"# Plan\n- a\n- B\n");
    }
    let or = hub_core::text::oracle::evaluate(&ops, &blobs);
    assert_eq!(or.conflicts, evaluate_with(ops.iter(), &blobs).conflicts);
}

// ---------- hub-diff-line/1 golden vectors (7.12 #8) ----------

fn golden(base_text: &str, saved: &str) -> (Vec<(ElemId, u32)>, Vec<Ins>) {
    let s = id(1, 1);
    let doc = TextDoc::new(s, base_text.as_bytes());
    let all = |_: OpId| true;
    diff_line(&doc.visible_runs(&all), saved.as_bytes(), &|l| {
        doc.successor_at(l, &all)
    })
    .unwrap()
}

#[test]
fn diff_line_golden() {
    let s = id(1, 1);
    // Swap: the tie goes to the deletion first ("a" deleted, re-inserted).
    assert_eq!(
        golden("a\nb\n", "b\na\n"),
        (vec![(e(s, 0), 2)], vec![ins(Some(e(s, 3)), None, "a\n")])
    );
    // Replace in the middle: left = last deleted element.
    assert_eq!(
        golden("x\ny\nz\n", "x\nY\nz\n"),
        (
            vec![(e(s, 2), 2)],
            vec![ins(Some(e(s, 3)), Some(e(s, 4)), "Y\n")]
        )
    );
    // Insert at top after a BOM pseudo-line.
    assert_eq!(
        golden("\u{feff}a\n", "\u{feff}n\na\n"),
        (vec![], vec![ins(Some(e(s, 0)), Some(e(s, 3)), "n\n")])
    );
    // Append after a final line without newline modifies that line.
    assert_eq!(
        golden("a\nb", "a\nb\nc\n"),
        (vec![(e(s, 2), 1)], vec![ins(Some(e(s, 2)), None, "b\nc\n")])
    );
    // \r\n kept per line; duplicate lines align leftmost.
    assert_eq!(golden("a\r\na\r\n", "a\r\n"), (vec![(e(s, 3), 3)], vec![]));
    // Pure insert at START.
    assert_eq!(
        golden("a\n", "z\na\n"),
        (vec![], vec![ins(None, Some(e(s, 0)), "z\n")])
    );
    assert_eq!(golden("", "q"), (vec![], vec![ins(None, None, "q")]));
    // Determinism: same inputs, same output.
    assert_eq!(
        golden("1\n2\n3\n4\n", "4\n2\n3\n1\n"),
        golden("1\n2\n3\n4\n", "4\n2\n3\n1\n")
    );
    // No-op save: no op.
    let doc = TextDoc::new(s, b"same\n");
    let all = |_: OpId| true;
    assert!(diff_line(&doc.visible_runs(&all), b"same\n", &|l| doc
        .successor_at(l, &all))
    .is_none());
}

/// The D > 20 000 fallback is "delete all, insert all".
#[test]
fn diff_line_cost_fallback() {
    let a: Vec<String> = (0..20_001).map(|i| format!("a{i}\n")).collect();
    let b: Vec<String> = (0..20_001).map(|i| format!("b{i}\n")).collect();
    let (a, b) = (a.concat(), b.concat());
    let la = split_lines(a.as_bytes());
    let lb = split_lines(b.as_bytes());
    assert!(diff_lines(&la, &lb, DIFF_MAX_D).is_none());
    let s = id(1, 1);
    let (del, ins_) = golden(&a, &b);
    assert_eq!(del, vec![(e(s, 0), a.len() as u32)]);
    assert_eq!(ins_.len(), 1);
    assert_eq!(ins_[0].bytes, b.as_bytes());
    assert_eq!(ins_[0].left, Some(e(s, a.len() as u32 - 1)));
}
