//! Shared text-history generator (Issue #53, Text-CRDT §7.12): replicas
//! that author like `record`/`watch` do (7.5.1: deps = the checked-out
//! version `V_wd`, `seen` = content heads at `V_wd`, anchor mapping at
//! deps), receive remote ops without checking them out, flip a file with a
//! whole-file `SetContent`, delete the file or its folder at head, and
//! check out.

use std::collections::{BTreeMap, BTreeSet};

use hub_core::text::*;
use hub_core::tree::*;
use proptest::prelude::*;

pub type Blobs = BTreeMap<BlobId, Vec<u8>>;

pub struct TRep {
    pub rid: u128,
    pub known: BTreeMap<OpId, Op>,
    /// Checked-out version (causally closed).
    pub wd: BTreeSet<OpId>,
}

/// One authored TextEdit with the author's deps closure and saved bytes.
#[derive(Clone, Debug)]
pub struct Saved {
    pub op: OpId,
    pub saved: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct SimStats {
    /// Saves made while the author's store held a content op of the file
    /// in another epoch that was not checked out (7.4.3 stale-epoch save).
    pub stale_saves: usize,
    pub saves: usize,
    pub flips: usize,
    pub deletes: usize,
}

pub struct TSim {
    pub reps: Vec<TRep>,
    pub blobs: Blobs,
    pub dir: NodeId,
    pub file: NodeId,
    pub saved: Vec<Saved>,
    pub stats: SimStats,
}

pub fn heads(ids: &BTreeSet<OpId>, ops: &BTreeMap<OpId, Op>) -> BTreeSet<OpId> {
    let mut h = ids.clone();
    for i in ids {
        for d in &ops[i].deps {
            h.remove(d);
        }
    }
    h
}

/// Line vocabulary: multibyte characters, `\r\n`, a BOM appears via a
/// separate mutation.
const LINES: [&str; 8] = [
    "a\n", "b\n", "é\n", "한글\n", "x\r\n", "a\n", "zz\n", "🙂\n",
];

/// Mutate `base` by one line-level edit chosen by `(k, a, b)`.
pub fn mutate_text(base: &[u8], k: u8, a: u8, b: u8) -> Vec<u8> {
    let lines = split_lines(base);
    let mut v: Vec<Vec<u8>> = lines.iter().map(|l| l.to_vec()).collect();
    let bom = v.first().is_some_and(|l| l == &[0xEF, 0xBB, 0xBF]);
    let lo = usize::from(bom);
    let n = v.len();
    let pick = |x: u8, m: usize| if m == 0 { 0 } else { x as usize % m };
    let line = LINES[b as usize % LINES.len()].as_bytes().to_vec();
    match k % 9 {
        0 | 1 => {
            let p = lo + pick(a, n - lo + 1);
            v.insert(p.min(v.len()), line);
        }
        2 if n > lo => {
            let p = lo + pick(a, n - lo);
            v.remove(p);
        }
        3 if n > lo => {
            let p = lo + pick(a, n - lo);
            v[p] = line;
        }
        4 => {
            // Last line without final newline: append / extend.
            match v.last_mut() {
                Some(l) if !l.ends_with(b"\n") => l.push(b'q'),
                _ => v.push(b"tail".to_vec()),
            }
        }
        5 => {
            if bom {
                v.remove(0);
            } else {
                v.insert(0, vec![0xEF, 0xBB, 0xBF]);
            }
        }
        6 if n > lo + 1 => {
            let p = lo + pick(a, n - lo - 1);
            v.swap(p, p + 1);
        }
        7 if n > lo => {
            // Two-line replace.
            let p = lo + pick(a, n - lo);
            v[p] = line.clone();
            v.insert(p, LINES[(b as usize + 1) % LINES.len()].as_bytes().to_vec());
        }
        _ => {}
    }
    // Keep only the last line without newline.
    let mut out = Vec::new();
    let len = v.len();
    for (i, mut l) in v.into_iter().enumerate() {
        if i + 1 < len && !l.ends_with(b"\n") && l != [0xEF, 0xBB, 0xBF] {
            l.push(b'\n');
        }
        out.extend(l);
    }
    out
}

impl TSim {
    pub fn new(nrep: usize, seed: &[u8]) -> Self {
        let mut blobs = Blobs::new();
        blobs.insert(BlobId::of(seed), seed.to_vec());
        let mut sim = TSim {
            reps: (1..=nrep as u128)
                .map(|rid| TRep {
                    rid,
                    known: BTreeMap::new(),
                    wd: BTreeSet::new(),
                })
                .collect(),
            blobs,
            dir: NodeId::new(1, 1),
            file: NodeId::new(2, 1),
            saved: Vec::new(),
            stats: SimStats::default(),
        };
        let d = sim.mint_at_head(
            0,
            Payload::Create {
                kind: Kind::Directory,
                parent: ROOT,
                name: "d".into(),
            },
        );
        let f = sim.mint_at_head(
            0,
            Payload::Create {
                kind: Kind::File,
                parent: d,
                name: "f.txt".into(),
            },
        );
        sim.dir = d;
        sim.file = f;
        sim.mint_at_head(
            0,
            Payload::SetContent {
                node: f,
                blob: BlobId::of(seed),
            },
        );
        for r in 1..nrep {
            sim.sync(0, r);
            sim.checkout(r);
        }
        sim
    }

    fn next_counter(&self, r: usize) -> u64 {
        1 + self.reps[r]
            .known
            .keys()
            .map(|k| k.counter)
            .max()
            .unwrap_or(0)
    }

    fn mint(&mut self, r: usize, payload: Payload, deps: BTreeSet<OpId>) -> OpId {
        let id = NodeId::new(self.next_counter(r), self.reps[r].rid);
        let rep = &mut self.reps[r];
        rep.known.insert(id, Op { id, deps, payload });
        rep.wd.insert(id);
        id
    }

    /// Author at head (mutations, deletes): check out first.
    pub fn mint_at_head(&mut self, r: usize, payload: Payload) -> OpId {
        self.checkout(r);
        let rep = &self.reps[r];
        let deps = heads(&rep.wd, &rep.known);
        self.mint(r, payload, deps)
    }

    pub fn checkout(&mut self, r: usize) {
        let rep = &mut self.reps[r];
        rep.wd = rep.known.keys().copied().collect();
    }

    pub fn sync(&mut self, a: usize, b: usize) {
        let mut all = self.reps[a].known.clone();
        all.extend(self.reps[b].known.clone());
        self.reps[a].known = all.clone();
        self.reps[b].known = all;
    }

    pub fn wd_ops(&self, r: usize) -> BTreeMap<OpId, Op> {
        let rep = &self.reps[r];
        rep.wd.iter().map(|i| (*i, rep.known[i].clone())).collect()
    }

    /// The text of the file in replica `r`'s working directory, or `None`
    /// if the file is not live there.
    pub fn wd_text(&self, r: usize) -> Option<Vec<u8>> {
        let ops = self.wd_ops(r);
        let ev = evaluate_with(ops.values(), &self.blobs);
        if !is_live(&ev.state, self.file) {
            return None;
        }
        let blob = ev.state[&self.file].content?;
        match ev.text.content.get(&self.file) {
            Some(nt) => nt.bytes.clone(),
            None => self.blobs.get(&blob).cloned(),
        }
    }

    /// Save `text` in replica `r` (record/watch path, 7.5.1).
    pub fn save(&mut self, r: usize, text: &[u8]) -> Option<OpId> {
        let ops = self.wd_ops(r);
        let ev = evaluate_with(ops.values(), &self.blobs);
        if !is_live(&ev.state, self.file) {
            return None;
        }
        let edit = author_text_edit(&ops, &ev, &self.blobs, self.file, text)?;
        // Stale-epoch save: the store holds an unchecked-out content op of
        // the file in another epoch.
        let rep = &self.reps[r];
        let stale = rep.known.iter().any(|(id, o)| {
            !rep.wd.contains(id)
                && match &o.payload {
                    Payload::SetContent { node, .. } => *node == self.file && *id != edit.epoch,
                    Payload::TextEdit(t) => t.node == self.file && t.epoch != edit.epoch,
                    _ => false,
                }
        });
        self.stats.stale_saves += stale as usize;
        self.stats.saves += 1;
        let deps = heads(&rep.wd, &rep.known);
        let id = self.mint(r, Payload::TextEdit(edit), deps);
        self.saved.push(Saved {
            op: id,
            saved: text.to_vec(),
        });
        Some(id)
    }

    /// Whole-file save of `text` (flip / old peer), on the wd base.
    pub fn flip(&mut self, r: usize, text: &[u8]) -> OpId {
        let blob = BlobId::of(text);
        self.blobs.insert(blob, text.to_vec());
        self.stats.flips += 1;
        let rep = &self.reps[r];
        let deps = heads(&rep.wd, &rep.known);
        self.mint(
            r,
            Payload::SetContent {
                node: self.file,
                blob,
            },
            deps,
        )
    }

    pub fn all_ops(&self) -> Vec<Op> {
        let mut all = BTreeMap::new();
        for r in &self.reps {
            all.extend(r.known.clone());
        }
        all.into_values().collect()
    }

    /// Run one scripted action.
    pub fn act(&mut self, (r, act, a, b): (u8, u8, u8, u8)) {
        let r = r as usize % self.reps.len();
        match act % 16 {
            0..=6 => {
                if let Some(t) = self.wd_text(r) {
                    let t2 = mutate_text(&t, (act / 16).wrapping_add(a), a, b);
                    self.save(r, &t2);
                }
            }
            7 | 8 => {
                let o = (r + 1 + a as usize) % self.reps.len();
                if o != r {
                    self.sync(r, o);
                }
            }
            9 | 10 => self.checkout(r),
            11 => {
                if let Some(t) = self.wd_text(r) {
                    let t2 = mutate_text(&t, a, b, a);
                    self.flip(r, &t2);
                }
            }
            12 => {
                let node = if a % 2 == 0 { self.file } else { self.dir };
                self.stats.deletes += 1;
                self.mint_at_head(r, Payload::Delete { node });
            }
            13 => {
                // Restore the folder and file at head (explicit move back).
                self.mint_at_head(
                    r,
                    Payload::Move {
                        node: self.dir,
                        new_parent: ROOT,
                        new_name: "d".into(),
                    },
                );
                if b % 2 == 0 {
                    self.mint_at_head(
                        r,
                        Payload::Move {
                            node: self.file,
                            new_parent: self.dir,
                            new_name: "f.txt".into(),
                        },
                    );
                }
            }
            _ => {
                // Save twice in a row (a causally later edit inside runs).
                for k in 0..2u8 {
                    if let Some(t) = self.wd_text(r) {
                        let t2 = mutate_text(&t, a.wrapping_add(k), b, a ^ k);
                        self.save(r, &t2);
                    }
                }
            }
        }
    }
}

pub const SEEDS: [&str; 4] = [
    "# Plan\n- a\n- b\n",
    "x\ny\n",
    "",
    "\u{feff}one\r\ntwo\nthree",
];

/// Build a history from a script.
pub fn text_history(nrep: usize, seed: usize, script: &[(u8, u8, u8, u8)]) -> TSim {
    let mut sim = TSim::new(nrep, SEEDS[seed % SEEDS.len()].as_bytes());
    for &s in script {
        sim.act(s);
    }
    sim
}

/// `(replicas, seed text index, script, delivery seeds)`.
pub type TextCase = (usize, usize, Vec<(u8, u8, u8, u8)>, Vec<u64>);
/// Every element of a doc with its deleters, per `(node, epoch)`.
pub type DocLists = BTreeMap<(NodeId, OpId), Vec<(ElemId, Vec<OpId>)>>;

pub fn text_strategy() -> impl Strategy<Value = TextCase> {
    (
        2usize..=3,
        0usize..4,
        prop::collection::vec(any::<(u8, u8, u8, u8)>(), 1..28),
        prop::collection::vec(any::<u64>(), 3),
    )
}

pub struct Xr(pub u64);
impl Xr {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// A random causal order of `ops` (each op after its deps in the set).
pub fn causal_order(ops: &[Op], seed: u64) -> Vec<OpId> {
    let mut rng = Xr(seed | 1);
    let ids: BTreeSet<OpId> = ops.iter().map(|o| o.id).collect();
    let mut left: Vec<&Op> = ops.iter().collect();
    let mut done: BTreeSet<OpId> = BTreeSet::new();
    let mut out = Vec::new();
    while !left.is_empty() {
        let ready: Vec<usize> = (0..left.len())
            .filter(|&i| {
                left[i]
                    .deps
                    .iter()
                    .all(|d| !ids.contains(d) || done.contains(d))
            })
            .collect();
        let i = ready[rng.below(ready.len())];
        let o = left.swap_remove(i);
        done.insert(o.id);
        out.push(o.id);
    }
    out
}

/// Add invalid TextEdits (7.4.1/7.4.2) authored by replica 99 on top of
/// `ops`; returns the new ops.
pub fn invalid_text_ops(ops: &[Op], file: NodeId, blobs: &Blobs, seed: u64) -> Vec<Op> {
    let mut rng = Xr(seed | 1);
    let map: BTreeMap<OpId, Op> = ops.iter().map(|o| (o.id, o.clone())).collect();
    let all: BTreeSet<OpId> = map.keys().copied().collect();
    let hs = heads(&all, &map);
    let max = map.keys().map(|k| k.counter).max().unwrap_or(0);
    // A valid-looking epoch: the greatest SetContent of the file.
    let epoch = map
        .values()
        .rev()
        .find(|o| matches!(&o.payload, Payload::SetContent { node, .. } if *node == file))
        .map(|o| o.id)
        .unwrap();
    let texts: Vec<OpId> = map
        .values()
        .filter(|o| matches!(o.payload, Payload::TextEdit(_)))
        .map(|o| o.id)
        .collect();
    let mut out = Vec::new();
    for k in 0..3u64 {
        let e = |op: OpId, off: u32| ElemId { op, off };
        let mut t = TextEdit {
            node: file,
            epoch,
            seen: BTreeSet::new(),
            del: vec![],
            ins: vec![Ins {
                left: None,
                right: None,
                bytes: b"ok\n".to_vec(),
            }],
        };
        let mut deps = hs.clone();
        match rng.below(8) {
            0 => t.ins[0].bytes = vec![0xff, b'\n'],
            1 => t.ins[0].bytes = Vec::new(),
            // Not an epoch: the directory's Create.
            2 => t.epoch = NodeId::new(1, 1),
            3 => t.del = vec![(e(epoch, 0), 0)],
            4 => t.del = vec![(e(epoch, 1_000_000), 1)],
            5 => {
                t.seen.insert(NodeId::new(max + 50, 77));
            }
            6 => {
                // An element of an op not in deps*: depend on nothing.
                deps = BTreeSet::new();
                deps.insert(epoch);
                if let Some(&x) = texts.last() {
                    t.ins[0].left = Some(e(x, 0));
                } else {
                    t.node = NodeId::new(max + 60, 78);
                }
            }
            _ => t.ins[0].left = Some(e(NodeId::new(max + 70, 79), 0)),
        }
        out.push(Op {
            id: NodeId::new(max + 1 + k, 99),
            deps,
            payload: Payload::TextEdit(t),
        });
    }
    // Issue #53 repair R1: deletes that end or start inside a multibyte
    // character (é, 한, 🙂, BOM) of a seed run or of a TextEdit run.
    let mut k = 3u64;
    let mut cands: Vec<(OpId, OpId, Vec<u8>)> = Vec::new(); // (epoch, elem op, bytes)
    for o in map.values() {
        match &o.payload {
            Payload::SetContent { node, blob } if *node == file => {
                if let Some(b) = blobs.get(blob) {
                    cands.push((o.id, o.id, b.clone()));
                }
            }
            Payload::TextEdit(t) if t.node == file && o.id.replica != ReplicaId(99) => {
                let b: Vec<u8> = t.ins.iter().flat_map(|i| i.bytes.iter().copied()).collect();
                cands.push((t.epoch, o.id, b));
            }
            _ => {}
        }
    }
    let mut kinds = 0;
    for (epoch, x, b) in cands.iter().rev() {
        let Some(p) = b.iter().position(|&c| c >= 0xC0) else {
            continue;
        };
        for (off, len) in [(p as u32, 1u32), (p as u32 + 1, 1)] {
            out.push(Op {
                id: NodeId::new(max + 1 + k, 99),
                deps: hs.clone(),
                payload: Payload::TextEdit(TextEdit {
                    node: file,
                    epoch: *epoch,
                    seen: BTreeSet::new(),
                    del: vec![(ElemId { op: *x, off }, len)],
                    ins: vec![],
                }),
            });
            k += 1;
        }
        kinds += 1;
        if kinds == 2 {
            break;
        }
    }
    out
}

/// Every element of every doc, for order comparisons.
pub fn doc_lists(ev: &Evaluation) -> DocLists {
    ev.text
        .docs
        .iter()
        .filter_map(|(k, d)| match d {
            DocSlot::Doc(d) => Some((
                *k,
                d.elements()
                    .into_iter()
                    .map(|(e, _, del)| (e, del))
                    .collect(),
            )),
            DocSlot::Deferred => None,
        })
        .collect()
}
