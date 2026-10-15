//! Test-only text oracle (Issue #53): Text-CRDT §7 restated literally and
//! naively, as the sorted-fold reference for the equivalence proptests.
//!
//! * One tree node per character (no runs), materialized `causal_past`.
//! * The insert side is decided by the 7.3.2 wording itself: "if `left`
//!   has no right children in the author's state at `deps`, right child of
//!   `left`; otherwise left child of `right`", evaluated on the elements
//!   whose op is in `deps*(o)` (not by the receiver's subtree test that
//!   [`super::TextDoc`] uses).
//! * The list is rebuilt by a full in-order traversal after every op.
//! * Content, conflicts (7.4.3), notices (7.8) by direct quantification.
//!
//! O(n²) or worse; small histories only.

use std::collections::{BTreeMap, BTreeSet};

use super::{epoch_blob, fetch, BlobSource, ElemId, Notice, TextEdit};
use crate::tree::{causal_past, BlobId, Conflict, NodeId, Op, OpId, Outcome, Payload};

#[derive(Clone, Debug)]
struct ONode {
    parent: Option<ElemId>,
    right_side: bool,
    ro: Option<ElemId>,
}

/// One epoch, character by character.
#[derive(Clone, Debug, Default)]
pub struct OracleDoc {
    nodes: BTreeMap<ElemId, ONode>,
    bytes: BTreeMap<OpId, Vec<u8>>,
    /// Full list (tombstones included).
    pub list: Vec<ElemId>,
    pub deleted: BTreeMap<ElemId, BTreeSet<OpId>>,
}

fn clen(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 6 {
        2
    } else if b >> 4 == 14 {
        3
    } else {
        4
    }
}

fn chars_of(op: OpId, bytes: &[u8]) -> Vec<ElemId> {
    let mut v = Vec::new();
    let mut p = 0;
    while p < bytes.len() {
        v.push(ElemId { op, off: p as u32 });
        p += clen(bytes[p]);
    }
    v
}

impl OracleDoc {
    fn new(epoch: OpId, seed: &[u8]) -> Self {
        let mut d = OracleDoc::default();
        d.bytes.insert(epoch, seed.to_vec());
        let mut prev = None;
        for e in chars_of(epoch, seed) {
            d.nodes.insert(
                e,
                ONode {
                    parent: prev,
                    right_side: true,
                    ro: None,
                },
            );
            prev = Some(e);
        }
        d.rebuild();
        d
    }

    pub fn char_bytes(&self, e: ElemId) -> &[u8] {
        let b = &self.bytes[&e.op];
        &b[e.off as usize..e.off as usize + clen(b[e.off as usize])]
    }

    fn is_elem(&self, e: ElemId) -> bool {
        self.nodes.contains_key(&e)
    }

    /// In-order traversal; right siblings by right origin (later first,
    /// by the previous list's positions) then id, left siblings by id.
    fn rebuild(&mut self) {
        let pos: BTreeMap<ElemId, usize> =
            self.list.iter().enumerate().map(|(i, e)| (*e, i)).collect();
        let key = |ro: Option<ElemId>| ro.map_or(usize::MAX, |r| pos[&r]);
        let mut lefts: BTreeMap<Option<ElemId>, Vec<ElemId>> = BTreeMap::new();
        let mut rights: BTreeMap<Option<ElemId>, Vec<ElemId>> = BTreeMap::new();
        for (e, n) in &self.nodes {
            if n.right_side {
                rights.entry(n.parent).or_default().push(*e);
            } else {
                lefts.entry(n.parent).or_default().push(*e);
            }
        }
        for v in rights.values_mut() {
            v.sort_by(|a, b| {
                let (ka, kb) = (key(self.nodes[a].ro), key(self.nodes[b].ro));
                kb.cmp(&ka).then(a.cmp(b))
            });
        }
        // Iterative in-order: (node, expanded).
        let mut out = Vec::new();
        let mut stack: Vec<(Option<ElemId>, bool)> = vec![(None, false)];
        while let Some((x, done)) = stack.pop() {
            if done {
                if let Some(e) = x {
                    out.push(e);
                }
                continue;
            }
            for c in rights.get(&x).into_iter().flatten().rev() {
                stack.push((Some(*c), false));
            }
            stack.push((x, true));
            for c in lefts.get(&x).into_iter().flatten().rev() {
                stack.push((Some(*c), false));
            }
        }
        self.list = out;
    }

    fn insert(&mut self, id: OpId, t: &TextEdit, past: &BTreeSet<OpId>) {
        let mut all = Vec::new();
        for i in &t.ins {
            all.extend_from_slice(&i.bytes);
        }
        self.bytes.insert(id, all);
        let mut off = 0u32;
        for i in &t.ins {
            let chars = chars_of(id, &i.bytes);
            let has_right = self
                .nodes
                .iter()
                .any(|(e, n)| n.parent == i.left && n.right_side && past.contains(&e.op));
            let (parent, right_side) = match (has_right, i.right) {
                (true, Some(r)) => (Some(r), false),
                _ => (i.left, true),
            };
            let mut prev: Option<ElemId> = None;
            for c in chars {
                let c = ElemId {
                    op: id,
                    off: c.off + off,
                };
                let n = match prev {
                    None => ONode {
                        parent,
                        right_side,
                        ro: i.right,
                    },
                    Some(p) => ONode {
                        parent: Some(p),
                        right_side: true,
                        ro: i.right,
                    },
                };
                self.nodes.insert(c, n);
                prev = Some(c);
            }
            off += i.bytes.len() as u32;
        }
        self.rebuild();
        for &(e, len) in &t.del {
            for c in chars_of(e.op, &self.bytes[&e.op]) {
                if c.off >= e.off && c.off < e.off + len {
                    self.deleted.entry(c).or_default().insert(id);
                }
            }
        }
    }

    pub fn materialize(&self, in_v: &dyn Fn(OpId) -> bool) -> Vec<u8> {
        let mut out = Vec::new();
        for e in &self.list {
            let del = self
                .deleted
                .get(e)
                .is_some_and(|d| d.iter().any(|x| in_v(*x)));
            if in_v(e.op) && !del {
                out.extend_from_slice(self.char_bytes(*e));
            }
        }
        out
    }

    /// Successor of `left` among elements whose op is in `in_v`.
    pub fn successor(&self, left: Option<ElemId>, in_v: &dyn Fn(OpId) -> bool) -> Option<ElemId> {
        let mut it = self.list.iter().filter(|e| in_v(e.op));
        if let Some(l) = left {
            it.by_ref().find(|e| **e == l)?;
        }
        it.next().copied()
    }

    pub fn contains_op(&self, op: OpId) -> bool {
        self.bytes.contains_key(&op)
    }
}

/// Oracle result.
#[derive(Clone, Debug, Default)]
pub struct OracleText {
    pub conflicts: BTreeSet<Conflict>,
    pub valid: BTreeSet<OpId>,
    /// Head bytes of nodes with a valid TextEdit, and the winning epoch.
    pub content: BTreeMap<NodeId, (OpId, Vec<u8>)>,
    pub notices: BTreeSet<Notice>,
    pub docs: BTreeMap<(NodeId, OpId), OracleDoc>,
}

fn valid_utf8_run(b: &[u8]) -> bool {
    !b.is_empty() && std::str::from_utf8(b).is_ok()
}

/// The reference: sorted fold, char-level, literal rules. Every epoch
/// blob must be in `blobs` (no deferred mode).
pub fn evaluate(ops: &[Op], blobs: &dyn BlobSource) -> OracleText {
    let map: BTreeMap<OpId, Op> = ops.iter().map(|o| (o.id, o.clone())).collect();
    let past = causal_past(&map);
    let all_text: BTreeSet<OpId> = map
        .values()
        .filter(|o| matches!(o.payload, Payload::TextEdit(_)))
        .map(|o| o.id)
        .collect();
    let base = crate::tree::oracle::evaluate_text_aware(map.values(), &all_text);
    let mut docs: BTreeMap<(NodeId, OpId), OracleDoc> = BTreeMap::new();
    let mut valid = BTreeSet::new();
    for (id, op) in &map {
        let Payload::TextEdit(t) = &op.payload else {
            continue;
        };
        if base.outcomes[id] != Outcome::Applied {
            continue;
        }
        let p = &past[id];
        let Some(blob) = epoch_blob(&map, &base.outcomes, t.node, t.epoch) else {
            continue;
        };
        if !p.contains(&t.epoch) || !t.seen.iter().all(|s| p.contains(s)) {
            continue;
        }
        let seed = fetch(blobs, &blob).expect("oracle needs every epoch blob");
        if std::str::from_utf8(seed).is_err() {
            continue;
        }
        let d = docs
            .entry((t.node, t.epoch))
            .or_insert_with(|| OracleDoc::new(t.epoch, seed));
        let elem_ok = |e: &Option<ElemId>| e.is_none_or(|e| d.is_elem(e) && p.contains(&e.op));
        let ins_ok = t
            .ins
            .iter()
            .all(|i| valid_utf8_run(&i.bytes) && elem_ok(&i.left) && elem_ok(&i.right));
        let del_ok = t.del.iter().all(|&(e, len)| {
            let Some(b) = d.bytes.get(&e.op) else {
                return false;
            };
            let end = e.off as usize + len as usize;
            len > 0
                && p.contains(&e.op)
                && d.is_elem(e)
                && end <= b.len()
                && (end == b.len()
                    || d.is_elem(ElemId {
                        op: e.op,
                        off: end as u32,
                    }))
        });
        if !(ins_ok && del_ok) {
            continue;
        }
        d.insert(*id, t, p);
        valid.insert(*id);
    }
    let ev = crate::tree::oracle::evaluate_text_aware(map.values(), &valid);
    let mut out = OracleText {
        conflicts: ev.conflicts,
        valid: valid.clone(),
        ..Default::default()
    };
    let text = |id: &OpId| match &map[id].payload {
        Payload::TextEdit(t) => Some(t),
        _ => None,
    };
    let epoch_of = |id: &OpId| text(id).map_or(*id, |t| t.epoch);
    let nodes: BTreeSet<NodeId> = valid.iter().map(|v| text(v).unwrap().node).collect();
    for node in nodes {
        let cs: Vec<OpId> = map
            .iter()
            .filter(|(id, o)| match &o.payload {
                Payload::SetContent { node: n, .. } => {
                    *n == node && ev.outcomes[*id] == Outcome::Applied
                }
                Payload::TextEdit(t) => t.node == node && valid.contains(id),
                _ => false,
            })
            .map(|(id, _)| *id)
            .collect();
        let w = *cs.last().unwrap();
        let we = epoch_of(&w);
        let blob_of = |x: OpId| -> BlobId {
            match &map[&x].payload {
                Payload::SetContent { blob, .. } => *blob,
                Payload::TextEdit(t) => {
                    let d = &docs[&(node, t.epoch)];
                    BlobId::of(&d.materialize(&|o| o == x || past[&x].contains(&o)))
                }
                _ => unreachable!(),
            }
        };
        let head = match docs.get(&(node, we)) {
            Some(d) => d.materialize(&|_| true),
            None => {
                let b = match &map[&we].payload {
                    Payload::SetContent { blob, .. } => *blob,
                    _ => BlobId::empty(),
                };
                fetch(blobs, &b).unwrap().to_vec()
            }
        };
        out.content.insert(node, (we, head));
        for &e in &cs {
            if e == w || epoch_of(&e) == we {
                continue;
            }
            let overwritten = cs.iter().any(|&c| {
                past[&c].contains(&e)
                    && match text(&c) {
                        None => true,
                        Some(t) => {
                            t.seen.contains(&e) || t.seen.iter().any(|s| past[s].contains(&e))
                        }
                    }
            });
            if !overwritten {
                out.conflicts.insert(Conflict::Content {
                    node,
                    winner: w,
                    loser: e,
                    winner_blob: blob_of(w),
                    loser_blob: blob_of(e),
                });
            }
        }
        // Notices (7.8), by definition.
        let Some(d) = docs.get(&(node, we)) else {
            continue;
        };
        let group: Vec<OpId> = cs
            .iter()
            .copied()
            .filter(|c| text(c).is_some_and(|t| t.epoch == we))
            .collect();
        let touch = |o: OpId| -> BTreeSet<ElemId> {
            let t = text(&o).unwrap();
            let mut s = BTreeSet::new();
            for &(e, len) in &t.del {
                for c in chars_of(e.op, &d.bytes[&e.op]) {
                    if c.off >= e.off && c.off < e.off + len {
                        s.insert(c);
                    }
                }
            }
            let mut off = 0u32;
            for i in &t.ins {
                s.extend(i.left);
                s.extend(i.right);
                let first = ElemId { op: o, off };
                let cs = chars_of(o, &i.bytes);
                let last = ElemId {
                    op: o,
                    off: off + cs.last().unwrap().off,
                };
                let pf = d.list.iter().position(|e| *e == first).unwrap();
                let pl = d.list.iter().position(|e| *e == last).unwrap();
                if pf > 0 {
                    s.insert(d.list[pf - 1]);
                }
                if let Some(n) = d.list.get(pl + 1) {
                    s.insert(*n);
                }
                off += i.bytes.len() as u32;
            }
            s
        };
        let touches: Vec<BTreeSet<ElemId>> = group.iter().map(|&g| touch(g)).collect();
        let n = group.len();
        let mut comp: Vec<usize> = (0..n).collect();
        let edge = |a: usize, b: usize| {
            let (ta, tb) = (text(&group[a]).unwrap(), text(&group[b]).unwrap());
            let concurrent =
                !past[&group[a]].contains(&group[b]) && !past[&group[b]].contains(&group[a]);
            let identical = ta.del == tb.del && ta.ins == tb.ins;
            let overlap = !touches[a].is_disjoint(&touches[b]);
            concurrent && !identical && overlap
        };
        // Connected components by repeated relabeling.
        loop {
            let mut changed = false;
            for a in 0..n {
                for b in 0..n {
                    if a != b && edge(a, b) && comp[a] != comp[b] {
                        let m = comp[a].min(comp[b]);
                        comp[a] = m;
                        comp[b] = m;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        let mut groups: BTreeMap<usize, Vec<OpId>> = BTreeMap::new();
        for k in 0..n {
            groups.entry(comp[k]).or_default().push(group[k]);
        }
        for g in groups.into_values() {
            if g.len() < 2 {
                continue;
            }
            if cs.iter().any(|c| g.iter().all(|x| past[c].contains(x))) {
                continue;
            }
            out.notices.insert(Notice::ConcurrentEdit {
                node,
                lines: oracle_lines(d, &g),
                ops: g,
            });
        }
    }
    out.docs = docs;
    out
}

/// Lines (1-based) of head text: visible chars of the group's inserts; a
/// hidden group char (inserted or deleted by the group) counts at the
/// line of the next visible char (clamped to the last line).
fn oracle_lines(d: &OracleDoc, g: &[OpId]) -> (u32, u32) {
    let set: BTreeSet<OpId> = g.iter().copied().collect();
    let head = d.materialize(&|_| true);
    let nl = head.iter().filter(|&&b| b == b'\n').count() as u32;
    let total = (nl + u32::from(!head.is_empty() && *head.last().unwrap() != b'\n')).max(1);
    let mut line = 1u32;
    let (mut lo, mut hi) = (u32::MAX, 0u32);
    for e in &d.list {
        let dels = d.deleted.get(e);
        let visible = dels.is_none_or(|s| s.is_empty());
        let mine = set.contains(&e.op) || dels.is_some_and(|s| s.iter().any(|x| set.contains(x)));
        let b = d.char_bytes(*e);
        if mine {
            let l = if visible { line } else { line.min(total) };
            lo = lo.min(l);
            hi = hi.max(l);
        }
        if visible && b == b"\n" {
            line += 1;
        }
    }
    if hi == 0 {
        (1, 1)
    } else {
        (lo, hi)
    }
}
