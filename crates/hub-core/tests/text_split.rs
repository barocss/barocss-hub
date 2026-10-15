//! Issue #54 (Planner decision on oversized saves, Text-CRDT 7.4.1
//! "chained saves"): `split_text_edit` turns one save into a chain of
//! causally ordered TextEdits, each within a record-size limit.
//!
//! Properties: every op fits; with no concurrent ops the chain's head
//! equals the unsplit edit's head (= the saved text); in any concurrent
//! context every chain op is valid, the text of the concurrent ops is
//! exactly the text they have next to the unsplit edit, our inserted
//! bytes appear in the same order as the unsplit edit's, the chain never
//! raises a notice among its own ops, and anchors stay adjacent (7.12
//! #17) within the chain. Plus an 8 MiB single-line run.

use std::collections::{BTreeMap, BTreeSet};

use hub_core::text::*;
use hub_core::tree::*;
use proptest::prelude::*;

mod common;
use common::text::*;

/// A stand-in record size: fixed envelope + base64 of the bytes + ids.
fn size(o: &Op) -> usize {
    let Payload::TextEdit(t) = &o.payload else {
        return 64;
    };
    96 + 48 * (o.deps.len() + t.seen.len())
        + 64 * t.del.len()
        + t.ins
            .iter()
            .map(|i| 96 + i.bytes.len().div_ceil(3) * 4)
            .sum::<usize>()
}

fn heads_of(ids: &BTreeMap<OpId, Op>) -> BTreeSet<OpId> {
    let deps: BTreeSet<OpId> = ids.values().flat_map(|o| o.deps.iter().copied()).collect();
    ids.keys().filter(|k| !deps.contains(k)).copied().collect()
}

/// Visible (undeleted) elements of the winning doc, with their op.
fn visible(ev: &Evaluation, ops: &BTreeMap<OpId, Op>, node: NodeId) -> Vec<(OpId, Vec<u8>)> {
    let e = ev.text.content[&node].epoch;
    let d = ev.text.doc(node, e).unwrap();
    let seed = d.seed().to_vec();
    d.elements()
        .into_iter()
        .filter(|(_, _, del)| del.is_empty())
        .map(|(x, len, _)| {
            let b = if x.op == e {
                seed[x.off as usize..(x.off + len) as usize].to_vec()
            } else {
                let Payload::TextEdit(t) = &ops[&x.op].payload else {
                    unreachable!()
                };
                let all: Vec<u8> = t.ins.iter().flat_map(|i| i.bytes.clone()).collect();
                all[x.off as usize..(x.off + len) as usize].to_vec()
            };
            (x.op, b)
        })
        .collect()
}

proptest! {
    #[test]
    fn chain_equals_the_unsplit_edit((nrep, seed, script, seeds) in text_strategy(), extra in 1usize..40, limit in 500usize..1500) {
        let sim = text_history(nrep, seed, &script);
        let r = (seeds[0] % nrep as u64) as usize;
        let Some(cur) = sim.wd_text(r) else { return Ok(()) };
        let base = sim.wd_ops(r);
        let ev = evaluate_with(base.values(), &sim.blobs);
        // A big save: many new lines and one long multi-byte line, plus a
        // random mutation.
        let mut saved = mutate_text(&cur, seeds[1] as u8, seeds[2] as u8, 3);
        for k in 0..extra {
            saved.extend_from_slice(format!("added line {k} é한🙂\n").as_bytes());
        }
        saved.extend("x한".repeat(extra * 20).as_bytes());
        let Some(te) = author_text_edit(&base, &ev, &sim.blobs, sim.file, &saved) else { return Ok(()) };
        let all = sim.all_ops();
        let maxc = all.iter().map(|o| o.id.counter).max().unwrap();
        let first = NodeId::new(maxc + 1, 77);
        let deps = heads_of(&base);
        let unsplit = Op { id: first, deps: deps.clone(), payload: Payload::TextEdit(te.clone()) };
        let chain = split_text_edit(first, deps, te, &size, limit).expect("splits");
        prop_assert!(chain.len() > 1 || size(&chain[0]) <= limit);
        for (k, o) in chain.iter().enumerate() {
            prop_assert!(size(o) <= limit, "op {} of {} too big", k, chain.len());
            if k > 0 {
                prop_assert_eq!(&o.deps, &[chain[k - 1].id].into());
                let Payload::TextEdit(t) = &o.payload else { unreachable!() };
                prop_assert_eq!(&t.seen, &[chain[k - 1].id].into());
            }
        }
        // No concurrent ops: head = saved text, as for the unsplit edit.
        let mut alone: BTreeMap<OpId, Op> = base.clone();
        alone.extend(chain.iter().map(|o| (o.id, o.clone())));
        let ev_c = evaluate_with(alone.values(), &sim.blobs);
        prop_assert_eq!(ev_c.text.content[&sim.file].bytes.clone().unwrap(), saved.clone());
        // 7.12 #17 inside the chain: each continuation's right is the
        // successor of its left at its deps.
        for o in &chain[1..] {
            let Payload::TextEdit(t) = &o.payload else { unreachable!() };
            let d = ev_c.text.doc(sim.file, t.epoch).unwrap();
            let inv = |x: OpId| x == o.id || causal_past(&alone)[&o.id].contains(&x);
            for i in &t.ins {
                if i.left.is_some_and(|l| chain.iter().any(|c| c.id == l.op)) {
                    let succ = d.successor_at(i.left, &|x| inv(x) && x != o.id);
                    prop_assert_eq!(succ, i.right);
                }
            }
        }
        // Concurrent context: every other op of the simulation.
        let mut with_c: BTreeMap<OpId, Op> = all.iter().map(|o| (o.id, o.clone())).collect();
        let mut with_u = with_c.clone();
        with_c.extend(chain.iter().map(|o| (o.id, o.clone())));
        with_u.insert(unsplit.id, unsplit.clone());
        let ec = evaluate_with(with_c.values(), &sim.blobs);
        let eu = evaluate_with(with_u.values(), &sim.blobs);
        for o in &chain {
            prop_assert!(ec.text.valid.contains(&o.id), "chain op invalid");
        }
        prop_assert!(eu.text.valid.contains(&unsplit.id));
        let ids: BTreeSet<OpId> = chain.iter().map(|o| o.id).collect();
        if ec.text.content[&sim.file].epoch == eu.text.content[&sim.file].epoch
            && ec.text.doc(sim.file, ec.text.content[&sim.file].epoch).is_some()
        {
            let vc = visible(&ec, &with_c, sim.file);
            let vu = visible(&eu, &with_u, sim.file);
            let others = |v: &[(OpId, Vec<u8>)], mine: &dyn Fn(OpId) -> bool| -> Vec<u8> {
                v.iter().filter(|(o, _)| !mine(*o)).flat_map(|(_, b)| b.clone()).collect()
            };
            let ours = |v: &[(OpId, Vec<u8>)], mine: &dyn Fn(OpId) -> bool| -> Vec<u8> {
                v.iter().filter(|(o, _)| mine(*o)).flat_map(|(_, b)| b.clone()).collect()
            };
            let in_chain = |o: OpId| ids.contains(&o);
            let is_u = |o: OpId| o == unsplit.id;
            prop_assert_eq!(others(&vc, &in_chain), others(&vu, &is_u));
            prop_assert_eq!(ours(&vc, &in_chain), ours(&vu, &is_u));
        }
        // The chain alone never raises a notice (its ops are causally
        // ordered); with concurrent ops a notice always names one of them.
        let quiet = ev_c.text.notices.iter().all(|Notice::ConcurrentEdit { ops, .. }| !ops.iter().any(|o| ids.contains(o)));
        prop_assert!(quiet);
        for Notice::ConcurrentEdit { ops, .. } in &ec.text.notices {
            prop_assert!(ops.iter().any(|o| !ids.contains(o)));
        }
        let past = causal_past(&with_c);
        // Authoring on the base (7.12 #18, Online-First 6.10 #20): every
        // chain op's causal past is the author's base plus earlier chain ops.
        for o in &chain {
            prop_assert!(past[&o.id].iter().all(|x| base.contains_key(x) || ids.contains(x)));
        }
        for a in &chain {
            for b in &chain {
                prop_assert!(a.id == b.id || past[&a.id].contains(&b.id) || past[&b.id].contains(&a.id));
            }
        }
    }
}

/// An 8 MiB single-line run (the 7.1 text cap) splits into ops that each
/// fit, and the chain reproduces it byte for byte.
#[test]
fn eight_mib_single_line_run() {
    let e = NodeId::new(1, 1);
    let line: Vec<u8> = "abc한🙂".repeat(8 * 1024 * 1024 / 10).into_bytes();
    let te = TextEdit {
        node: e,
        epoch: e,
        seen: BTreeSet::new(),
        del: vec![],
        ins: vec![Ins {
            left: None,
            right: None,
            bytes: line.clone(),
        }],
    };
    let limit = 192 * 1024;
    let chain = split_text_edit(NodeId::new(2, 1), [e].into(), te, &size, limit).unwrap();
    assert!(chain.len() > 40, "{}", chain.len());
    let mut doc = TextDoc::new(e, b"");
    for o in &chain {
        assert!(size(o) <= limit);
        let Payload::TextEdit(t) = &o.payload else {
            unreachable!()
        };
        assert!(doc.check(t, &|_| true));
        doc.apply(o.id, t);
    }
    assert_eq!(doc.materialize_head(), line);
}
