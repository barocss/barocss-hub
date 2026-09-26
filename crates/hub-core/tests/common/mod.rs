//! Shared #5 history generator (moved from convergence.rs for #9).
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use hub_core::tree::*;
use proptest::prelude::*;

// ---------- authoring simulator ----------

struct Author {
    rid: u128,
    counter: u64,
    known: BTreeMap<OpId, Op>,
}

impl Author {
    fn new(rid: u128) -> Self {
        Author {
            rid,
            counter: 1,
            known: BTreeMap::new(),
        }
    }

    fn heads(&self) -> BTreeSet<OpId> {
        let mut h: BTreeSet<OpId> = self.known.keys().copied().collect();
        for o in self.known.values() {
            for d in &o.deps {
                h.remove(d);
            }
        }
        h
    }

    /// Mint an op with Lamport counter over the current frontier (§2.1).
    fn mint(&mut self, payload: Payload) -> Op {
        let deps = self.heads();
        let maxdep = deps.iter().map(|d| d.counter).max().unwrap_or(0);
        self.counter = 1 + self.counter.max(maxdep);
        let op = Op {
            id: NodeId::new(self.counter, self.rid),
            deps,
            payload,
        };
        self.known.insert(op.id, op.clone());
        op
    }

    fn state(&self) -> State {
        evaluate(self.known.values()).state
    }
}

const NAMES: [&str; 3] = ["n", "m", "k"];

/// Deterministically turn an action script into a multi-replica op history.
pub fn simulate(nrep: usize, script: &[(u8, u8, u8, u8)]) -> Vec<Op> {
    let mut reps: Vec<Author> = (1..=nrep as u128).map(Author::new).collect();
    for &(r, act, a, b) in script {
        let r = r as usize % nrep;
        let st = reps[r].state();
        let nodes: Vec<NodeId> = st.keys().copied().filter(|n| !n.is_reserved()).collect();
        let dirs: Vec<NodeId> = st
            .values()
            .filter(|n| n.kind == Kind::Directory || n.kind == Kind::Root)
            .map(|n| n.id)
            .collect();
        let pick = |v: &Vec<NodeId>, i: u8| v[i as usize % v.len()];
        let name = NAMES[b as usize % NAMES.len()].to_string();
        match act % 13 {
            0 | 1 => {
                let kind = if b % 3 == 0 {
                    Kind::File
                } else {
                    Kind::Directory
                };
                let parent = pick(&dirs, a);
                reps[r].mint(Payload::Create { kind, parent, name });
            }
            2..=4 if !nodes.is_empty() => {
                let node = pick(&nodes, a);
                let new_parent = pick(&dirs, b);
                // Authoring-time cycle rejection (§2.2, row 8). Moves into
                // trashed directories are allowed.
                if !is_ancestor_or_self(&st, node, new_parent) {
                    reps[r].mint(Payload::Move {
                        node,
                        new_parent,
                        new_name: name,
                    });
                }
            }
            5 | 6 if !nodes.is_empty() => {
                let node = pick(&nodes, a);
                reps[r].mint(Payload::Delete { node });
            }
            7 | 8 => {
                // sync r <- other
                let o = (r + 1 + a as usize % (nrep - 1).max(1)) % nrep;
                if o != r {
                    let ops: Vec<Op> = reps[o].known.values().cloned().collect();
                    for op in ops {
                        reps[r].known.insert(op.id, op);
                    }
                }
            }
            9 if !nodes.is_empty() && b % 4 == 0 => {
                // Buggy peer: statically invalid op (File parent / bad name).
                let node = pick(&nodes, a);
                let files: Vec<NodeId> = st
                    .values()
                    .filter(|n| n.kind == Kind::File)
                    .map(|n| n.id)
                    .collect();
                let (new_parent, new_name) = if files.is_empty() {
                    (ROOT, "a/b".to_string())
                } else {
                    (pick(&files, b), "f".to_string())
                };
                reps[r].mint(Payload::Move {
                    node,
                    new_parent,
                    new_name,
                });
            }
            10 if nrep > 1 => {
                // Concurrent cross-move of two directories (cycle candidate):
                // r moves u under v while r' (unsynced) moves v under u.
                let o = (r + 1) % nrep;
                let sto = reps[o].state();
                let ud: Vec<NodeId> = dirs
                    .iter()
                    .copied()
                    .filter(|n| !n.is_reserved() && sto.contains_key(n))
                    .collect();
                if ud.len() >= 2 {
                    let u = pick(&ud, a);
                    let v = pick(&ud, a / 2 + 1);
                    if u != v && !is_ancestor_or_self(&st, u, v) && !is_ancestor_or_self(&sto, v, u)
                    {
                        reps[r].mint(Payload::Move {
                            node: u,
                            new_parent: v,
                            new_name: name.clone(),
                        });
                        reps[o].mint(Payload::Move {
                            node: v,
                            new_parent: u,
                            new_name: name,
                        });
                    }
                }
            }
            11 if nrep > 1 => {
                // Delete of a directory concurrent with moving one of its
                // strict descendants elsewhere (revived candidate).
                let o = (r + 1) % nrep;
                let sto = reps[o].state();
                let pairs: Vec<(NodeId, NodeId)> = nodes
                    .iter()
                    .flat_map(|&x| dirs.iter().map(move |&d| (d, x)))
                    .filter(|&(d, x)| {
                        !d.is_reserved()
                            && d != x
                            && is_ancestor_or_self(&st, d, x)
                            && is_live(&st, x)
                            && sto.contains_key(&x)
                    })
                    .collect();
                if !pairs.is_empty() {
                    let (d, x) = pairs[a as usize % pairs.len()];
                    let dests: Vec<NodeId> = sto
                        .values()
                        .filter(|n| {
                            n.kind != Kind::File
                                && is_live(&sto, n.id)
                                && !is_ancestor_or_self(&sto, x, n.id)
                                && !is_ancestor_or_self(&sto, d, n.id)
                        })
                        .map(|n| n.id)
                        .collect();
                    if !dests.is_empty() {
                        reps[r].mint(Payload::Delete { node: d });
                        let np = dests[b as usize / 4 % dests.len()];
                        reps[o].mint(Payload::Move {
                            node: x,
                            new_parent: np,
                            new_name: name,
                        });
                    }
                }
            }
            12 if nrep > 1 => {
                // Concurrent create of the same name in the same live
                // directory (§2.7 row 4, name-collision candidate; #9).
                let o = (r + 1) % nrep;
                let sto = reps[o].state();
                let common: Vec<NodeId> = dirs
                    .iter()
                    .copied()
                    .filter(|&d| is_live(&st, d) && is_live(&sto, d))
                    .collect();
                if !common.is_empty() {
                    let parent = pick(&common, a);
                    let kind = if b % 2 == 0 {
                        Kind::File
                    } else {
                        Kind::Directory
                    };
                    reps[r].mint(Payload::Create {
                        kind,
                        parent,
                        name: name.clone(),
                    });
                    reps[o].mint(Payload::Create { kind, parent, name });
                }
            }
            _ => {}
        }
    }
    let mut all = BTreeMap::new();
    for rep in reps {
        all.extend(rep.known);
    }
    all.into_values().collect()
}

/// Shuffle with duplicates, using a small xorshift from `seed`.
pub fn delivery(ops: &[Op], seed: u64) -> Vec<Op> {
    let mut s = seed | 1;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let mut v: Vec<Op> = ops.to_vec();
    let dups = ops.len() / 2 + 1;
    for _ in 0..dups {
        if !ops.is_empty() {
            v.push(ops[next() as usize % ops.len()].clone());
        }
    }
    for i in (1..v.len()).rev() {
        let j = next() as usize % (i + 1);
        v.swap(i, j);
    }
    v
}

pub type Script = Vec<(u8, u8, u8, u8)>;

pub fn script_strategy() -> impl Strategy<Value = (usize, Script, Vec<u64>)> {
    (
        2usize..=3,
        prop::collection::vec(any::<(u8, u8, u8, u8)>(), 1..40),
        prop::collection::vec(any::<u64>(), 3),
    )
}
