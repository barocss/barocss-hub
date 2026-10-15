//! Issue #53: Text-CRDT §7.12 properties as proptests over the shared
//! text generator (`common::text`), checked against the char-level
//! literal oracle (`hub_core::text::oracle`).
//!
//! Property -> test:
//! * #1 convergence (any causal order, incremental = sorted fold):
//!   `p01_p14_convergence_and_oracle`
//! * #2 no lost insertions: `p02_p03_p04_p05_list_properties`
//! * #3 delete precision: `p02_p03_p04_p05_list_properties`
//! * #4 intention (FugueMax): `p02_p03_p04_p05_list_properties`, R2 case
//!   `p04_r2_remote_insert_inside_deps`
//! * #5 non-interleaving (concurrent-only wording):
//!   `p02_p03_p04_p05_list_properties`
//! * #6 maximal non-interleaving vs a brute-force oracle:
//!   `p06_matches_brute_force_oracle`, `p06_forward_backward_runs`
//! * #7 sequential equivalence: `p07_sequential_equivalence`
//! * #8 diff determinism: `p08_diff_determinism` (+ golden vectors in
//!   `text_example.rs`)
//! * #9 virtual seed agreement: `p09_virtual_seed_agreement`
//! * #10 epoch reduction: `p10_epoch_reduction` (+ `content.rs` unchanged)
//! * #11 revive: `p11_revive`
//! * #12 materialize at any version: `p12_materialize_any_version`
//! * #13 UTF-8 closure, invalid ops: `p13_utf8_and_invalid_ops`
//! * #14 notice determinism: `p01_p14_convergence_and_oracle`
//! * #16 stale-epoch save visible: `p16_stale_epoch_save`
//! * #17 anchor adjacency: `p17_anchor_adjacency`
//! * coverage floors: `text_generator_coverage_floor`
//! * Issue #54: incremental `Replica` (and its v2 snapshot) = evaluate for
//!   text sets: `replica_and_snapshot_equal_evaluate`

use std::collections::{BTreeMap, BTreeSet};

use hub_core::text::oracle::OracleText;
use hub_core::text::*;
use hub_core::tree::*;
use proptest::prelude::*;

mod common;
use common::text::*;
use common::{script_strategy, simulate};

fn build(nrep: usize, seed: usize, script: &[(u8, u8, u8, u8)], inv: u64) -> (TSim, Vec<Op>) {
    let sim = text_history(nrep, seed, script);
    let mut ops = sim.all_ops();
    if inv % 3 == 0 {
        ops.extend(invalid_text_ops(&ops, sim.file, &sim.blobs, inv));
    }
    (sim, ops)
}

fn map_of(ops: &[Op]) -> BTreeMap<OpId, Op> {
    ops.iter().map(|o| (o.id, o.clone())).collect()
}

fn text_of(o: &Op) -> Option<&TextEdit> {
    match &o.payload {
        Payload::TextEdit(t) => Some(t),
        _ => None,
    }
}

/// Head bytes of every text node.
fn heads_of(ev: &Evaluation) -> BTreeMap<NodeId, (OpId, Option<Vec<u8>>)> {
    ev.text
        .content
        .iter()
        .map(|(n, c)| (*n, (c.epoch, c.bytes.clone())))
        .collect()
}

fn same_as_oracle(ev: &Evaluation, or: &OracleText) -> Result<(), TestCaseError> {
    prop_assert_eq!(&ev.conflicts, &or.conflicts);
    prop_assert_eq!(&ev.text.valid, &or.valid);
    prop_assert_eq!(&ev.text.notices, &or.notices);
    let got: BTreeMap<NodeId, (OpId, Vec<u8>)> = ev
        .text
        .content
        .iter()
        .map(|(n, c)| (*n, (c.epoch, c.bytes.clone().unwrap())))
        .collect();
    prop_assert_eq!(&got, &or.content);
    for (k, d) in &or.docs {
        let mine = ev.text.doc(k.0, k.1).expect("doc");
        let a: Vec<(ElemId, Vec<OpId>)> = mine
            .elements()
            .into_iter()
            .map(|(e, _, del)| (e, del))
            .collect();
        let b: Vec<(ElemId, Vec<OpId>)> = d
            .list
            .iter()
            .map(|e| {
                (
                    *e,
                    d.deleted
                        .get(e)
                        .map(|s| s.iter().copied().collect())
                        .unwrap_or_default(),
                )
            })
            .collect();
        prop_assert_eq!(a, b, "list of {:?}", k);
    }
    Ok(())
}

proptest! {
    /// #1 + #14: content, conflicts, outcomes, notices and the full lists
    /// are equal for the sorted fold, random causal integration orders and
    /// the oracle.
    #[test]
    fn p01_p14_convergence_and_oracle((nrep, seed, script, seeds) in text_strategy()) {
        let (sim, ops) = build(nrep, seed, &script, seeds[0]);
        let blobs = &sim.blobs;
        let ev = evaluate_with(ops.iter(), blobs);
        for &s in &seeds {
            let order = causal_order(&ops, s);
            let ev2 = evaluate_in_order(&ops, blobs, &order);
            prop_assert_eq!(&ev.state, &ev2.state);
            prop_assert_eq!(&ev.conflicts, &ev2.conflicts);
            prop_assert_eq!(&ev.outcomes, &ev2.outcomes);
            prop_assert_eq!(&ev.text.notices, &ev2.text.notices);
            prop_assert_eq!(heads_of(&ev), heads_of(&ev2));
            prop_assert_eq!(doc_lists(&ev), doc_lists(&ev2));
        }
        let or = hub_core::text::oracle::evaluate(&ops, blobs);
        same_as_oracle(&ev, &or)?;
    }

    /// #2, #3, #4, #5 on the final lists.
    #[test]
    fn p02_p03_p04_p05_list_properties((nrep, seed, script, seeds) in text_strategy()) {
        let (sim, ops) = build(nrep, seed, &script, seeds[0]);
        let ev = evaluate_with(ops.iter(), &sim.blobs);
        let map = map_of(&ops);
        let past = causal_past(&map);
        for ((node, epoch), slot) in &ev.text.docs {
            let DocSlot::Doc(d) = slot else { continue };
            let els = d.elements();
            let pos: BTreeMap<ElemId, usize> = els.iter().enumerate().map(|(i, x)| (x.0, i)).collect();
            let valid: Vec<&Op> = ops
                .iter()
                .filter(|o| ev.text.valid.contains(&o.id) && text_of(o).is_some_and(|t| t.node == *node && t.epoch == *epoch))
                .collect();
            // Deletes from payloads (independent of the doc).
            let covered = |x: ElemId| {
                valid.iter().any(|o| {
                    text_of(o).unwrap().del.iter().any(|&(e, len)| e.op == x.op && x.off >= e.off && x.off < e.off + len)
                })
            };
            for (x, _, del) in &els {
                // #3 delete precision (and #2 for inserted elements).
                prop_assert_eq!(!del.is_empty(), covered(*x), "element {:?}", x);
            }
            for o in &valid {
                let t = text_of(o).unwrap();
                let mut off = 0u32;
                for i in &t.ins {
                    let n = i.bytes.len() as u32;
                    let first = ElemId { op: o.id, off };
                    let mut p = 0;
                    let mut last = first;
                    while p < n {
                        let e = ElemId { op: o.id, off: off + p };
                        // #2 every inserted element is in the list.
                        prop_assert!(pos.contains_key(&e));
                        last = e;
                        p += els[pos[&e]].1;
                    }
                    let (pf, pl) = (pos[&first], pos[&last]);
                    // #4 after left, before right (both may be tombstones).
                    if let Some(l) = i.left {
                        prop_assert!(pos[&l] < pf, "left {:?} not before {:?}", l, first);
                    }
                    if let Some(r) = i.right {
                        prop_assert!(pl < pos[&r], "right {:?} not after {:?}", r, last);
                    }
                    // #5 no element of a concurrent op inside the run.
                    for (x, _, _) in &els[pf..=pl] {
                        if x.op != o.id {
                            let conc = !past[&x.op].contains(&o.id) && !past[&o.id].contains(&x.op);
                            prop_assert!(!conc, "run of {:?} interleaved by {:?}", o.id, x.op);
                        }
                    }
                    off += n;
                }
            }
        }
    }

    /// #6: the run-based integration equals the literal per-character
    /// 7.3.2 rule (author's state at deps) on small histories.
    #[test]
    fn p06_matches_brute_force_oracle((nrep, seed, script, seeds) in text_strategy()) {
        let (sim, ops) = build(nrep, seed, &script[..script.len().min(12)], seeds[1]);
        let ev = evaluate_with(ops.iter(), &sim.blobs);
        same_as_oracle(&ev, &hub_core::text::oracle::evaluate(&ops, &sim.blobs))?;
    }

    /// #7: one replica, random byte-exact saves (mixed `\n`/`\r\n`, BOM,
    /// missing final newline): head = last save after each save.
    #[test]
    fn p07_sequential_equivalence(seed in 0usize..4, edits in prop::collection::vec(any::<(u8, u8, u8)>(), 1..30)) {
        let mut sim = TSim::new(1, SEEDS[seed].as_bytes());
        let mut text = SEEDS[seed].as_bytes().to_vec();
        for (k, a, b) in edits {
            text = mutate_text(&text, k, a, b);
            sim.save(0, &text);
            prop_assert_eq!(sim.wd_text(0).unwrap(), text.clone());
            let ev = evaluate_with(sim.all_ops().iter(), &sim.blobs);
            if let Some(c) = ev.text.content.get(&sim.file) {
                prop_assert_eq!(c.bytes.clone().unwrap(), text.clone());
            }
        }
    }

    /// #8: the diff is a pure function of (base, saved): repeated calls,
    /// and docs with the same visible text but different histories give
    /// the same edit modulo element ids.
    #[test]
    fn p08_diff_determinism(seed in 0usize..4, edits in prop::collection::vec(any::<(u8, u8, u8)>(), 1..12), last in any::<(u8, u8, u8)>()) {
        let mut sim = TSim::new(1, SEEDS[seed].as_bytes());
        let mut text = SEEDS[seed].as_bytes().to_vec();
        for (k, a, b) in edits {
            text = mutate_text(&text, k, a, b);
            sim.save(0, &text);
        }
        let saved = mutate_text(&text, last.0, last.1, last.2);
        let ops = sim.wd_ops(0);
        let ev = evaluate_with(ops.values(), &sim.blobs);
        let a = author_text_edit(&ops, &ev, &sim.blobs, sim.file, &saved);
        let b = author_text_edit(&ops, &ev, &sim.blobs, sim.file, &saved);
        prop_assert_eq!(&a, &b);
        // Same text as a fresh single-run seed: same line script.
        let fresh = TextDoc::new(NodeId::new(1, 9), &text);
        let all = |_: OpId| true;
        let c = diff_line(&fresh.visible_runs(&all), &saved, &|l| fresh.successor_at(l, &all));
        let shape = |d: &Option<DiffEdit>| d.as_ref().map(|(del, ins)| {
            (del.iter().map(|x| x.1).sum::<u32>(), ins.iter().map(|i| i.bytes.clone()).collect::<Vec<_>>())
        });
        let a2 = a.map(|t| (t.del, t.ins));
        prop_assert_eq!(shape(&a2), shape(&c));
    }

    /// #9: two replicas' first TextEdits on the same pre-upgrade file
    /// reference the seed ids; the merge has no duplicated seed text.
    #[test]
    fn p09_virtual_seed_agreement(seed in 0usize..4, x in any::<(u8, u8, u8)>(), y in any::<(u8, u8, u8)>()) {
        let mut sim = TSim::new(2, SEEDS[seed].as_bytes());
        let base = SEEDS[seed].as_bytes().to_vec();
        let epoch = sim.reps[0].known.keys().copied().max().unwrap();
        let t1 = mutate_text(&base, x.0, x.1, x.2);
        let t2 = mutate_text(&base, y.0, y.1, y.2);
        let a = sim.save(0, &t1);
        let b = sim.save(1, &t2);
        sim.sync(0, 1);
        let ops = sim.all_ops();
        let ev = evaluate_with(ops.iter(), &sim.blobs);
        for id in [a, b].into_iter().flatten() {
            let t = text_of(&ops.iter().find(|o| o.id == id).unwrap().clone()).unwrap().clone();
            prop_assert_eq!(t.epoch, epoch);
            for i in &t.ins {
                for e in [i.left, i.right].into_iter().flatten() {
                    prop_assert_eq!(e.op, epoch);
                }
            }
            for (e, _) in &t.del {
                prop_assert_eq!(e.op, epoch);
            }
        }
        if a.is_some() || b.is_some() {
            let d = ev.text.doc(sim.file, epoch).unwrap();
            let seeds: Vec<ElemId> = d.elements().into_iter().map(|x| x.0).filter(|e| e.op == epoch).collect();
            let uniq: BTreeSet<ElemId> = seeds.iter().copied().collect();
            prop_assert_eq!(seeds.len(), uniq.len());
            // Seed bytes appear once: the seed elements are one copy.
            let seed_len: u32 = d.elements().iter().filter(|x| x.0.op == epoch).map(|x| x.1).sum();
            prop_assert_eq!(seed_len as usize, base.len());
        }
        prop_assert!(ev.conflicts.is_empty());
    }

    /// #10: with no TextEdit, the text-aware evaluation equals the §2.3
    /// #18 rule exactly (the pre-#53 oracle) on the shared #5 generator.
    #[test]
    fn p10_epoch_reduction((nrep, script, _) in script_strategy()) {
        let ops = simulate(nrep, &script);
        let blobs = BlobStore::new();
        let ev = evaluate_with(ops.iter(), &blobs);
        let or = hub_core::tree::oracle::evaluate(ops.iter());
        prop_assert_eq!(&ev.state, &or.state);
        prop_assert_eq!(&ev.conflicts, &or.conflicts);
        prop_assert_eq!(&ev.outcomes, &or.outcomes);
        prop_assert!(ev.text.content.is_empty() && ev.text.notices.is_empty());
    }

    /// #11: a TextEdit concurrent with a delete of the file or its folder
    /// revives it, and every concurrent edit is visible.
    #[test]
    fn p11_revive(seed in 0usize..4, x in any::<(u8, u8, u8)>(), y in any::<(u8, u8, u8)>(), folder in any::<bool>(), extra in any::<bool>()) {
        let mut sim = TSim::new(3, SEEDS[seed].as_bytes());
        let base = SEEDS[seed].as_bytes().to_vec();
        let a = sim.save(0, &mutate_text(&base, x.0, x.1, x.2));
        let b = if extra { sim.save(1, &mutate_text(&base, y.0, y.1, y.2)) } else { None };
        let node = if folder { sim.dir } else { sim.file };
        let d = sim.mint_at_head(2, Payload::Delete { node });
        for r in 0..2 {
            sim.sync(r, 2);
        }
        let ops = sim.all_ops();
        let ev = evaluate_with(ops.iter(), &sim.blobs);
        if let Some(a) = a.or(b) {
            prop_assert!(is_live(&ev.state, sim.file));
            let edit = a.min(b.unwrap_or(a));
            prop_assert!(ev.conflicts.contains(&Conflict::EditRevived { node: sim.file, edit, delete: d }), "{:?}", ev.conflicts);
            let head = ev.text.content[&sim.file].bytes.clone().unwrap();
            let doc = ev.text.doc(sim.file, ev.text.content[&sim.file].epoch).unwrap();
            // All inserted elements of the concurrent edits are visible
            // (nobody deleted them).
            for id in [a].into_iter().chain(b) {
                for (e, _, del) in doc.elements() {
                    if e.op == id {
                        prop_assert!(del.is_empty());
                    }
                }
            }
            prop_assert_eq!(head, doc.materialize_head());
        } else {
            prop_assert!(!is_live(&ev.state, sim.file));
        }
    }

    /// #12: materialize(epoch, V) on the full doc equals the content of a
    /// replica holding exactly V, for random causally closed V.
    #[test]
    fn p12_materialize_any_version((nrep, seed, script, seeds) in text_strategy()) {
        let (sim, ops) = build(nrep, seed, &script, 1);
        let ev = evaluate_with(ops.iter(), &sim.blobs);
        let order = causal_order(&ops, seeds[0]);
        let cut = (seeds[1] as usize) % (order.len() + 1);
        let v: BTreeSet<OpId> = order[..cut].iter().copied().collect();
        let sub: Vec<Op> = ops.iter().filter(|o| v.contains(&o.id)).cloned().collect();
        let evv = evaluate_with(sub.iter(), &sim.blobs);
        let in_v = |o: OpId| v.contains(&o);
        for ((n, e), slot) in &evv.text.docs {
            let DocSlot::Doc(dv) = slot else { continue };
            let full = ev.text.doc(*n, *e).unwrap();
            prop_assert_eq!(full.materialize(&in_v), dv.materialize_head());
        }
        for (n, c) in &evv.text.content {
            if let Some(full) = ev.text.doc(*n, c.epoch) {
                prop_assert_eq!(full.materialize(&in_v), c.bytes.clone().unwrap());
            }
        }
    }

    /// #13: head text is valid UTF-8; invalid TextEdits are `invalid-op`
    /// (no-op, never a loser) in every order.
    #[test]
    fn p13_utf8_and_invalid_ops((nrep, seed, script, seeds) in text_strategy()) {
        let sim = text_history(nrep, seed, &script);
        let mut ops = sim.all_ops();
        let bad = invalid_text_ops(&ops, sim.file, &sim.blobs, seeds[0]);
        ops.extend(bad.clone());
        let ev = evaluate_with(ops.iter(), &sim.blobs);
        let clean = evaluate_with(sim.all_ops().iter(), &sim.blobs);
        for o in &bad {
            let c = Conflict::InvalidOp { op: o.id };
            prop_assert!(ev.conflicts.contains(&c));
            prop_assert_eq!(&ev.outcomes[&o.id], &Outcome::Invalid);
        }
        // Invalid ops change nothing else.
        let others: BTreeSet<Conflict> = ev.conflicts.iter().filter(|c| !matches!(c, Conflict::InvalidOp { op } if bad.iter().any(|b| b.id == *op))).cloned().collect();
        prop_assert_eq!(others, clean.conflicts.clone());
        prop_assert_eq!(heads_of(&ev), heads_of(&clean));
        for c in ev.text.content.values() {
            prop_assert!(std::str::from_utf8(c.bytes.as_ref().unwrap()).is_ok());
        }
        let ev2 = evaluate_in_order(&ops, &sim.blobs, &causal_order(&ops, seeds[1]));
        prop_assert_eq!(&ev.conflicts, &ev2.conflicts);
    }

    /// #16: wd at S1, remote SetContent S2 not checked out, save: S2 is a
    /// content loser; after checking S2 out first, no conflict.
    #[test]
    fn p16_stale_epoch_save(seed in 0usize..4, x in any::<(u8, u8, u8)>(), y in any::<(u8, u8, u8)>()) {
        let base = SEEDS[seed].as_bytes().to_vec();
        for checkout_first in [false, true] {
            let mut sim = TSim::new(2, &base);
            let s2 = sim.flip(1, &mutate_text(&base, 0, y.1, y.2));
            sim.sync(0, 1);
            if checkout_first {
                sim.checkout(0);
            }
            let wd = sim.wd_text(0).unwrap();
            let mut t = mutate_text(&wd, x.0, x.1, x.2);
            if t == wd {
                t.extend_from_slice(b"\nsaved");
            }
            let save = sim.save(0, &t).unwrap();
            let ops = sim.all_ops();
            let ev = evaluate_with(ops.iter(), &sim.blobs);
            let content: Vec<&Conflict> = ev.conflicts.iter().filter(|c| matches!(c, Conflict::Content { .. })).collect();
            if checkout_first {
                prop_assert!(content.is_empty(), "{:?}", content);
                prop_assert_eq!(ev.text.content[&sim.file].bytes.clone().unwrap(), t);
            } else {
                // S2 is neither in deps*(save) nor in seen*(save).
                prop_assert_eq!(content.len(), 1);
                let Conflict::Content { winner, loser, loser_blob, .. } = content[0] else { unreachable!() };
                prop_assert_eq!(*winner, save);
                prop_assert_eq!(*loser, s2);
                prop_assert!(sim.blobs.contains_key(loser_blob));
            }
        }
    }

    /// #17: each authored `ins.right` is the successor of `ins.left` in the
    /// author's full list at deps (computed on the oracle's list).
    #[test]
    fn p17_anchor_adjacency((nrep, seed, script, _) in text_strategy()) {
        let sim = text_history(nrep, seed, &script);
        let ops = sim.all_ops();
        let map = map_of(&ops);
        let past = causal_past(&map);
        let or = hub_core::text::oracle::evaluate(&ops, &sim.blobs);
        for s in &sim.saved {
            let t = text_of(&map[&s.op]).unwrap();
            let Some(d) = or.docs.get(&(t.node, t.epoch)) else { continue };
            let p = &past[&s.op];
            let in_deps = |o: OpId| p.contains(&o);
            for i in &t.ins {
                prop_assert_eq!(i.right, d.successor(i.left, &in_deps), "op {:?}", s.op);
            }
        }
    }
}

/// #4 R2 case (7.5.1): a remote insert `X` landed right after the anchor
/// and is in the op's deps but not in the diff base (server-side diff,
/// hash-mismatch fallback): the saved run sits immediately after its
/// anchor and before `X`.
#[test]
fn p04_r2_remote_insert_inside_deps() {
    for seed in [0usize, 1, 3] {
        let base = SEEDS[seed].as_bytes().to_vec();
        let mut sim = TSim::new(2, &base);
        // B inserts X after line 0.
        let lines = split_lines(&base);
        let mut tb = lines.clone();
        tb.insert(1, b"X\n");
        let x = sim.save(1, &tb.concat()).unwrap();
        sim.sync(0, 1);
        // A's diff base: V_wd (without X); deps: V_wd ∪ {X}.
        let base_ops = sim.wd_ops(0);
        let ev_base = evaluate_with(base_ops.values(), &sim.blobs);
        let mut deps_ops = base_ops.clone();
        deps_ops.insert(x, sim.reps[0].known[&x].clone());
        let ev_deps = evaluate_with(deps_ops.values(), &sim.blobs);
        let mut ta = lines.clone();
        ta.insert(1, b"Y\n");
        let t = author_text_edit_on(
            &ev_deps,
            &base_ops,
            &ev_base,
            &sim.blobs,
            sim.file,
            &ta.concat(),
        )
        .unwrap();
        assert_eq!(
            t.ins[0].right,
            Some(ElemId { op: x, off: 0 }),
            "seed {seed}"
        );
        let id = NodeId::new(100, 1);
        let mut ops = sim.all_ops();
        ops.push(Op {
            id,
            deps: [x].into_iter().chain(base_ops.keys().copied()).collect(),
            payload: Payload::TextEdit(t),
        });
        let ev = evaluate_with(ops.iter(), &sim.blobs);
        let mut want = lines.clone();
        want.insert(1, b"Y\n");
        want.insert(2, b"X\n");
        assert_eq!(
            ev.text.content[&sim.file].bytes.clone().unwrap(),
            want.concat(),
            "seed {seed}"
        );
    }
}

/// #6: forward and backward typing of whole lines by two concurrent
/// replicas at the same position is never interleaved (Fugue's maximal
/// non-interleaving for forward and backward runs).
#[test]
fn p06_forward_backward_runs() {
    for seed in [0usize, 1, 2, 3] {
        for (fa, fb) in [(true, true), (true, false), (false, true), (false, false)] {
            // A BOM is pseudo-line 0: "top of file" is after it (7.1).
            let lo = usize::from(SEEDS[seed].starts_with('\u{feff}'));
            for at in lo..=split_lines(SEEDS[seed].as_bytes()).len() {
                let base = SEEDS[seed].as_bytes().to_vec();
                let mut sim = TSim::new(2, &base);
                for (r, fwd, tag) in [(0usize, fa, "A"), (1, fb, "B")] {
                    let mut lines: Vec<Vec<u8>> =
                        split_lines(&base).iter().map(|l| l.to_vec()).collect();
                    if let Some(l) = lines.last_mut() {
                        if !l.ends_with(b"\n") {
                            l.push(b'\n');
                            sim.save(r, &lines.concat());
                        }
                    }
                    for k in 0..4 {
                        let line = format!("{tag}{k}\n").into_bytes();
                        let p = if fwd { at + k } else { at };
                        lines.insert(p.min(lines.len()), line);
                        sim.save(r, &lines.concat());
                    }
                }
                sim.sync(0, 1);
                let ev = evaluate_with(sim.all_ops().iter(), &sim.blobs);
                let head =
                    String::from_utf8(ev.text.content[&sim.file].bytes.clone().unwrap()).unwrap();
                let tags: Vec<char> = head
                    .lines()
                    .filter_map(|l| l.chars().next())
                    .filter(|c| *c == 'A' || *c == 'B')
                    .collect();
                let switches = tags.windows(2).filter(|w| w[0] != w[1]).count();
                assert!(
                    switches <= 1,
                    "interleaved {head:?} (seed {seed}, at {at}, {fa}/{fb})"
                );
            }
        }
    }
}

// ---------- generator coverage floor ----------

/// Deterministic sample of the text strategy; guards against the generator
/// silently stopping to exercise the rare rules.
#[test]
fn text_generator_coverage_floor() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
    let mut runner = TestRunner::new_with_rng(
        Config::default(),
        TestRng::from_seed(RngAlgorithm::ChaCha, &[11; 32]),
    );
    let strat = text_strategy();
    let n = 512;
    let (mut same_pos, mut same_del, mut revive, mut flip, mut stale, mut invalid, mut notice) =
        (0, 0, 0, 0, 0, 0, 0);
    let mut midchar = 0;
    for _ in 0..n {
        let (nrep, seed, script, seeds) = strat.new_tree(&mut runner).unwrap().current();
        let (sim, ops) = build(nrep, seed, &script, seeds[0]);
        let ev = evaluate_with(ops.iter(), &sim.blobs);
        let map = map_of(&ops);
        let causal = Causality::new(&map);
        let valid: Vec<(&OpId, &TextEdit)> = map
            .iter()
            .filter(|(id, _)| ev.text.valid.contains(id))
            .map(|(id, o)| (id, text_of(o).unwrap()))
            .collect();
        let mut sp = false;
        let mut sd = false;
        for (i, (a, ta)) in valid.iter().enumerate() {
            for (b, tb) in &valid[i + 1..] {
                if !causal.concurrent(**a, **b) || ta.epoch != tb.epoch {
                    continue;
                }
                sp |= ta.ins.iter().any(|x| {
                    tb.ins
                        .iter()
                        .any(|y| x.left == y.left && x.right == y.right)
                });
                sd |= ta.del.iter().any(|&(e, l)| {
                    tb.del
                        .iter()
                        .any(|&(f, m)| e.op == f.op && e.off < f.off + m && f.off < e.off + l)
                });
            }
        }
        same_pos += sp as usize;
        same_del += sd as usize;
        revive += ev.conflicts.iter().any(
            |c| matches!(c, Conflict::EditRevived { edit, .. } if ev.text.valid.contains(edit)),
        ) as usize;
        let sets: Vec<OpId> = map
            .iter()
            .filter(|(id, o)| {
                matches!(o.payload, Payload::SetContent { .. })
                    && ev.outcomes[*id] == Outcome::Applied
            })
            .map(|(id, _)| *id)
            .collect();
        flip += valid.iter().any(|(t, te)| {
            sets.iter()
                .any(|s| *s != te.epoch && causal.concurrent(**t, *s))
        }) as usize;
        stale += (sim.stats.stale_saves > 0) as usize;
        invalid += ev
            .conflicts
            .iter()
            .any(|c| matches!(c, Conflict::InvalidOp { op } if text_of(&map[op]).is_some()))
            as usize;
        notice += (!ev.text.notices.is_empty()) as usize;
        midchar +=
            (invalid_text_ops(&sim.all_ops(), sim.file, &sim.blobs, seeds[0]).len() > 3) as usize;
    }
    eprintln!(
        "text coverage over {n} cases: same-position inserts={same_pos} same-range deletes={same_del} \
         revive={revive} flip={flip} stale-epoch saves={stale} invalid={invalid} notices={notice} mid-char deletes={midchar}"
    );
    assert!(
        same_pos * 10 >= n,
        "same-position inserts {same_pos}/{n} below 10%"
    );
    assert!(
        same_del * 20 >= n,
        "same-range deletes {same_del}/{n} below 5%"
    );
    assert!(revive * 50 >= n, "revive {revive}/{n} below 2%");
    assert!(flip * 20 >= n, "flip {flip}/{n} below 5%");
    assert!(stale * 20 >= n, "stale-epoch saves {stale}/{n} below 5%");
    assert!(
        invalid * 5 >= n,
        "invalid TextEdits {invalid}/{n} below 20%"
    );
    assert!(notice * 10 >= n, "notices {notice}/{n} below 10%");
    assert!(
        midchar * 5 >= n,
        "mid-character deletes {midchar}/{n} below 20%"
    );
}

/// Issue #54: everything the replica derives equals `evaluate_with`.
fn replica_same(r: &Replica, ops: &[Op], blobs: &Blobs) -> Result<(), TestCaseError> {
    let ev = evaluate_with(ops.iter(), blobs);
    let map = map_of(ops);
    prop_assert_eq!(r.state(), &ev.state);
    let c = r.conflicts(&map);
    prop_assert_eq!(c.as_ref(), Some(&ev.conflicts));
    prop_assert_eq!(r.text_notices(), ev.text.notices.clone());
    prop_assert_eq!(r.text_valid(), &ev.text.valid);
    let content: BTreeMap<NodeId, (OpId, BlobId)> = ev
        .text
        .content
        .iter()
        .map(|(n, t)| (*n, (t.epoch, t.blob)))
        .collect();
    prop_assert_eq!(r.text_content(), &content);
    // Docs of a decoded snapshot reference their seeds (Issue #54): load
    // them on a copy, as a reader would.
    let mut loaded = r.clone();
    loaded
        .load_text_docs(blobs)
        .map_err(|b| TestCaseError::fail(format!("seed {b:?}")))?;
    let r = &loaded;
    for ((node, epoch), slot) in &ev.text.docs {
        if let DocSlot::Doc(d) = slot {
            let rd = r.text_doc(*node, *epoch);
            prop_assert!(rd.is_some(), "doc {:?}", (node, epoch));
            prop_assert_eq!(rd.unwrap().elements(), d.elements());
        }
    }
    Ok(())
}

proptest! {
    /// Issue #54 (T2 carry-over (c)): the incremental `Replica` fed in
    /// random causal batches (duplicates included) equals `evaluate_with`
    /// after every batch, with no evaluate fallback; a snapshot taken
    /// after any batch decodes to a replica that keeps agreeing as more
    /// batches arrive (carry-over (b)). Also out-of-order delivery (ops
    /// before their deps are buffered).
    #[test]
    fn replica_and_snapshot_equal_evaluate((nrep, seed, script, seeds) in text_strategy()) {
        let (sim, ops) = build(nrep, seed, &script, seeds[0]);
        let blobs = &sim.blobs;
        let order = causal_order(&ops, seeds[1]);
        let by_id = map_of(&ops);
        let mut rng = Xr(seeds[2] | 1);
        let mut r = Replica::new();
        let mut got: Vec<Op> = Vec::new();
        let mut i = 0;
        while i < order.len() {
            let k = 1 + rng.below(4);
            let mut batch: Vec<Op> = order[i..(i + k).min(order.len())]
                .iter()
                .map(|id| by_id[id].clone())
                .collect();
            if rng.below(3) == 0 {
                batch.reverse();
            }
            if rng.below(4) == 0 && !got.is_empty() {
                batch.push(got[rng.below(got.len())].clone());
            }
            i += k;
            for o in &batch {
                if !got.iter().any(|g| g.id == o.id) {
                    got.push(o.clone());
                }
            }
            r.receive_all_with(batch, blobs);
            replica_same(&r, &got, blobs)?;
            if rng.below(3) == 0 {
                let snap = r.encode_snapshot().expect("closed replica");
                let back = Replica::decode_snapshot(&snap).map_err(TestCaseError::fail)?;
                prop_assert_eq!(back.encode_snapshot().unwrap(), snap);
                r = back;
                replica_same(&r, &got, blobs)?;
            }
        }
    }
}
