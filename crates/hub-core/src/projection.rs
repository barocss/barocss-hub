//! Filesystem projection (Repository-Semantics §1.5), Issue #9.
//!
//! `project` is a pure, deterministic function of the tree [`State`]: it walks
//! live nodes from `ROOT` and assigns each a relative path. No I/O.
//!
//! Choices where §1.5 is open (documented on Issue #9, not in the Wiki):
//! - Paths are `/`-joined relative to the working-tree root; `ROOT` is `""`.
//! - Collision groups are formed by exact byte equality of names (no case
//!   folding or Unicode normalization; §1.5 open question).
//! - Short id of a node = `<counter>-<low 32 bits of replica, 8 hex digits>`.
//!   Suffixed name = `name~<short-id>`.
//! - Suffix self-collision: plain names of every group winner are reserved
//!   first. A loser tries `name~<short-id>`, then `name~<counter>-<full
//!   32-hex replica>`, then that with `.1`, `.2`, ... until unused among its
//!   siblings. Losers are resolved in ascending NodeId order.

use std::collections::{BTreeMap, BTreeSet};

use crate::tree::{BlobId, Kind, NodeId, State, ROOT};

/// Structural name-collision conflict: `node` is projected under a
/// disambiguated name because `winner` (smallest NodeId) holds `name`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NameCollision {
    pub node: NodeId,
    pub parent: NodeId,
    pub name: String,
    pub winner: NodeId,
    pub projected_name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Projection {
    /// Path of every live node (ROOT = "").
    pub paths: BTreeMap<NodeId, String>,
    /// Inverse map path -> node (injective by construction).
    pub entries: BTreeMap<String, NodeId>,
    /// One conflict per suffixed node.
    pub collisions: BTreeSet<NameCollision>,
    /// Content of every live File (Issue #18).
    pub blobs: BTreeMap<NodeId, BlobId>,
}

pub fn short_id(n: NodeId) -> String {
    format!("{}-{:08x}", n.counter, n.replica.0 as u32)
}

fn long_id(n: NodeId) -> String {
    format!("{}-{:032x}", n.counter, n.replica.0)
}

pub fn project(state: &State) -> Projection {
    let mut children: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
    for n in state.values() {
        if let Some(p) = n.parent {
            children.entry(p).or_default().push(n.id);
        }
    }
    let mut out = Projection::default();
    out.paths.insert(ROOT, String::new());
    out.entries.insert(String::new(), ROOT);
    // Walk from ROOT only; TRASH subtrees are never reached.
    let mut stack = vec![ROOT];
    while let Some(dir) = stack.pop() {
        let dir_path = out.paths[&dir].clone();
        let kids = children.get(&dir).cloned().unwrap_or_default();
        // Group by name; `kids` is ascending by NodeId (BTreeMap order).
        let mut groups: BTreeMap<&str, Vec<NodeId>> = BTreeMap::new();
        for k in &kids {
            groups.entry(state[k].name.as_str()).or_default().push(*k);
        }
        let mut used: BTreeSet<String> = groups.keys().map(|s| s.to_string()).collect();
        let mut assigned: Vec<(NodeId, String)> = Vec::with_capacity(kids.len());
        let mut losers: Vec<(NodeId, NodeId)> = Vec::new();
        for (name, ids) in &groups {
            assigned.push((ids[0], name.to_string()));
            losers.extend(ids[1..].iter().map(|&l| (l, ids[0])));
        }
        losers.sort();
        for (l, winner) in losers {
            let name = &state[&l].name;
            let mut cand = format!("{name}~{}", short_id(l));
            if used.contains(&cand) {
                cand = format!("{name}~{}", long_id(l));
                let base = cand.clone();
                let mut i = 1u64;
                while used.contains(&cand) {
                    cand = format!("{base}.{i}");
                    i += 1;
                }
            }
            used.insert(cand.clone());
            out.collisions.insert(NameCollision {
                node: l,
                parent: dir,
                name: name.clone(),
                winner,
                projected_name: cand.clone(),
            });
            assigned.push((l, cand));
        }
        for (id, pname) in assigned {
            let path = if dir_path.is_empty() {
                pname
            } else {
                format!("{dir_path}/{pname}")
            };
            out.entries.insert(path.clone(), id);
            out.paths.insert(id, path);
            match state[&id].kind {
                Kind::Directory => stack.push(id),
                Kind::File => {
                    if let Some(b) = state[&id].content {
                        out.blobs.insert(id, b);
                    }
                }
                Kind::Root => {}
            }
        }
    }
    out
}
